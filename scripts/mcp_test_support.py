"""Shared helpers for isolated, real-EXE MCP checks; never launch mcp-host."""
import asyncio
from contextlib import AsyncExitStack
import json
import os
from pathlib import Path
import site
import sys

_dll_handles = []


def bootstrap_sdk(directory):
    if directory is None:
        return
    directory = directory.resolve()
    sys.path.insert(0, str(directory))
    site.addsitedir(str(directory))
    for sub in ("win32", "win32/lib", "pythonwin"):
        sys.path.insert(0, str(directory / sub))
    if os.name == "nt":
        import ctypes
        dll_dir = directory / "pywin32_system32"
        if dll_dir.is_dir():
            _dll_handles.append(os.add_dll_directory(str(dll_dir)))
            dll = dll_dir / f"pywintypes{sys.version_info.major}{sys.version_info.minor}.dll"
            if dll.is_file():
                _dll_handles.append(ctypes.WinDLL(str(dll)))


def write_config(directory):
    directory.mkdir(parents=True, exist_ok=True)
    path = (directory / "config.json").resolve()
    path.write_text(json.dumps({
        "schema_version": "1", "gateway": {"data_dir": str(directory / "data")},
        "mcp": {"port": 0}, "opcua": {"enabled": False},
        "channels": [], "poll_groups": [],
    }), encoding="utf-8")
    return path


def same_path(first, second):
    return os.path.samefile(first, second)


def endpoint(path):
    value = json.loads(Path(str(path) + ".mcp-endpoint.json").read_text(encoding="utf-8"))
    assert same_path(value["config"], path), "endpoint belongs to another configuration"
    return value


async def raw_status(path):
    # An authenticated monitoring request without initialize does not add an
    # artificial HTTP client to the list displayed by the configurator.
    import httpx
    owner = endpoint(path)
    async with httpx.AsyncClient(trust_env=False, timeout=3) as http:
        response = await http.post(owner["url"], headers={"Authorization": "Bearer " + owner["token"]}, json={
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "server_status", "arguments": {}},
        })
        response.raise_for_status()
        result = response.json()["result"]
        assert not result.get("isError"), result
        status = result["structuredContent"]
        assert status["instance_id"] == owner["instance_id"], "stale endpoint identity"
        return status


async def tool_status(session):
    result = await asyncio.wait_for(session.call_tool("server_status", {}), 10)
    assert not result.isError, result
    return result.structuredContent


def client_names(status):
    return sorted(client["name"] for client in status.get("mcp_clients", []))


async def offline_status(session, path):
    status = await tool_status(session)
    assert status.get("attached") is False and status["running"] is False, status
    assert same_path(status["config_path"], path), status
    assert status["config_id"] and status["instance_name"] and not status["mcp_clients"], status
    result = await asyncio.wait_for(session.call_tool("get_config", {}), 10)
    assert result.isError, "offline operational calls must not create a controller"
    assert any("config" in getattr(content, "text", "").lower() for content in result.content), result
    return status


async def open_client(stack: AsyncExitStack, server, path, name):
    from mcp import ClientSession, StdioServerParameters, types
    from mcp.client.stdio import stdio_client
    streams = await stack.enter_async_context(stdio_client(StdioServerParameters(
        command=str(server), args=["mcp", "--config", str(path)],
    )))
    session = await stack.enter_async_context(ClientSession(
        *streams, client_info=types.Implementation(name=name, version="1"),
    ))
    initialized = await asyncio.wait_for(session.initialize(), 10)
    return session, initialized


def info_matches(info, server, path):
    assert same_path(info["executable"], server), info
    assert same_path(info["config_path"], path), info
    assert info["config_id"] and info["instance_name"], info
    assert info["codex_toml"].startswith("[mcp_servers.")
    settings = json.loads(info["claude_json"])
    assert len(settings["mcpServers"]) == 1, settings
    target = next(iter(settings["mcpServers"].values()))
    assert same_path(target["command"], server), target
    assert target["args"][0:2] == ["mcp", "--config"], target
    assert same_path(target["args"][2], path), target


async def wait_for(operation, predicate, *, label, processes=(), timeout=30):
    import httpx
    deadline = asyncio.get_running_loop().time() + timeout
    last = None
    while True:
        for process in processes:
            assert process.returncode is None, f"{label}: owner process {process.pid} exited ({process.returncode})"
        try:
            last = await operation()
            if predicate(last):
                return last
        except (OSError, ValueError, httpx.HTTPError, KeyError) as error:
            last = repr(error)
        if asyncio.get_running_loop().time() >= deadline:
            raise AssertionError(f"{label}: readiness timeout; last observation: {last}")
        await asyncio.sleep(0.1)


async def spawn_owner(kind, server, gui, path):
    executable = gui if kind == "gui" else server
    arguments = ["--config", str(path)] if kind == "gui" else ["run", "--config", str(path)]
    return await asyncio.create_subprocess_exec(str(executable), *arguments,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)


async def stop_processes(processes):
    # The callers pass only PIDs created by this test, never a sidecar PID from
    # user configuration or a discovered process.
    for process in reversed(processes):
        if process.returncode is None:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
        try:
            await asyncio.wait_for(process.wait(), 10)
        except TimeoutError:
            process.kill()
            await process.wait()


async def ensure_no_hidden_host(path):
    if os.name != "nt":
        return  # These scripts exercise Windows product builds.
    # CIM/WMI can be unavailable in a restricted test process. Query command
    # lines with read-only native APIs, limited to this product's executables.
    import ctypes
    from ctypes import wintypes

    class ProcessEntry(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", wintypes.LONG),
            ("dwFlags", wintypes.DWORD), ("szExeFile", wintypes.WCHAR * 260),
        ]

    class UnicodeString(ctypes.Structure):
        _fields_ = [("length", wintypes.USHORT), ("maximum", wintypes.USHORT), ("buffer", ctypes.c_void_p)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    native = ctypes.WinDLL("ntdll")
    kernel.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    kernel.Process32FirstW.restype = wintypes.BOOL
    kernel.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    kernel.Process32NextW.restype = wintypes.BOOL
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    native.NtQueryInformationProcess.argtypes = [wintypes.HANDLE, wintypes.ULONG, ctypes.c_void_p, wintypes.ULONG, ctypes.POINTER(wintypes.ULONG)]
    native.NtQueryInformationProcess.restype = wintypes.LONG
    snapshot = kernel.CreateToolhelp32Snapshot(2, 0)  # TH32CS_SNAPPROCESS
    if snapshot == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    entry = ProcessEntry()
    entry.dwSize = ctypes.sizeof(entry)
    inventory = []
    try:
        present = kernel.Process32FirstW(snapshot, ctypes.byref(entry))
        while present:
            if entry.szExeFile.lower().startswith(("opc-modbus-server", "opc-modbus-config")):
                handle = kernel.OpenProcess(0x1000, False, entry.th32ProcessID)  # PROCESS_QUERY_LIMITED_INFORMATION
                if handle:
                    try:
                        size = wintypes.ULONG()
                        native.NtQueryInformationProcess(handle, 60, None, 0, ctypes.byref(size))
                        if 0 < size.value <= 1024 * 1024:
                            buffer = ctypes.create_string_buffer(size.value)
                            result = native.NtQueryInformationProcess(handle, 60, buffer, size, ctypes.byref(size))
                            if result >= 0:
                                value = ctypes.cast(buffer, ctypes.POINTER(UnicodeString)).contents
                                command = ctypes.wstring_at(value.buffer, value.length // 2)
                                if str(path).lower() in command.lower():
                                    inventory.append({"ProcessId": entry.th32ProcessID, "CommandLine": command})
                    finally:
                        kernel.CloseHandle(handle)
            present = kernel.Process32NextW(snapshot, ctypes.byref(entry))
    finally:
        kernel.CloseHandle(snapshot)
    assert inventory, "could not inspect this test's live stdio bridge process"
    assert all("mcp-host" not in item["CommandLine"].lower() for item in inventory), inventory
