"""Exercise real EXE stdio/HTTP transports with the official Python MCP SDK.

Client-only initialization must never start a controller. The console owner is
started explicitly for operational tools; all configurations contain no hardware.

python scripts/check-mcp.py --exe dist/windows/opc-modbus-server.exe \
    --sdk-dir target/mcp-sdk-test
"""
import argparse
import asyncio
from contextlib import AsyncExitStack
from pathlib import Path
import tempfile

from mcp_test_support import (
    bootstrap_sdk, client_names, endpoint, ensure_no_hidden_host, info_matches,
    offline_status, open_client, raw_status, spawn_owner, stop_processes,
    tool_status, wait_for, write_config,
)


async def concurrent_clients(server, path, owner_processes):
    """Six independently initialized client processes converge on the owner."""
    loop = asyncio.get_running_loop()
    offline = [loop.create_future() for _ in range(6)]
    attached = [loop.create_future() for _ in range(6)]
    owner_ready = asyncio.Event()
    release = asyncio.Event()

    async def client(index):
        try:
            async with AsyncExitStack() as stack:
                session, _ = await open_client(stack, server, path, f"concurrent-client-{index}")
                status = await offline_status(session, path)
                await session.send_ping()
                assert len((await session.list_tools()).tools) >= 30
                offline[index].set_result(status)
                await owner_ready.wait()
                status = await wait_for(lambda: tool_status(session), lambda s: s.get("attached") is True,
                    label=f"concurrent client {index}", processes=owner_processes)
                attached[index].set_result(status)
                await release.wait()
        except BaseException as error:
            for future in (offline[index], attached[index]):
                if not future.done():
                    future.set_exception(error)
            raise

    tasks = [asyncio.create_task(client(index)) for index in range(6)]
    completed = False
    try:
        statuses = await asyncio.wait_for(asyncio.gather(*offline), 20)
        assert len({s["config_id"] for s in statuses}) == 1, statuses
        assert not Path(str(path) + ".mcp-endpoint.json").exists()
        await ensure_no_hidden_host(path)
        console = await spawn_owner("server", server, None, path)
        owner_processes.append(console)
        await wait_for(lambda: raw_status(path), lambda s: s["running"], label="console startup", processes=owner_processes)
        assert endpoint(path)["pid"] == console.pid
        owner_ready.set()
        statuses = await asyncio.wait_for(asyncio.gather(*attached), 20)
        assert len({s["instance_id"] for s in statuses}) == 1, statuses
        expected = [f"concurrent-client-{index}" for index in range(6)]
        await wait_for(lambda: raw_status(path), lambda s: client_names(s) == expected,
            label="six concurrent registrations", processes=owner_processes)
        completed = True
    finally:
        release.set()
        if not completed:
            for task in tasks:
                task.cancel()
        results = await asyncio.gather(*tasks, return_exceptions=True)
        # Retrieve any unfinished phase exceptions even if initial setup failed.
        for future in offline + attached:
            if future.done() and not future.cancelled():
                future.exception()
    assert not any(isinstance(result, BaseException) for result in results), results
    await wait_for(lambda: raw_status(path), lambda s: not s["mcp_clients"],
        label="six client disconnects", processes=owner_processes)
    assert owner_processes[0].returncode is None, "client EOF stopped the explicit console owner"


async def exercise(server):
    from mcp import ClientSession, types
    from mcp.client.streamable_http import streamable_http_client
    import httpx

    assert server.is_file(), "build the selected EXE first"
    with tempfile.TemporaryDirectory(prefix="opc-mcp-sdk-") as directory:
        path = write_config(Path(directory))
        processes = []
        try:
            await concurrent_clients(server, path, processes)
            console = processes[0]
            async with AsyncExitStack() as stack:
                session, init = await open_client(stack, server, path, "official-sdk-main")
                assert init.serverInfo.name
                tools = await session.list_tools()
                assert len(tools.tools) >= 30
                await session.send_ping()
                config = (await session.call_tool("get_config", {})).structuredContent
                assert config["revision"]
                access = (await session.call_tool("check_access", {})).structuredContent
                assert access["ok"] and not access["hardware_accessed"], access
                preview = await session.call_tool("test_formula", {"formula": "raw * 2 + tag(\"x\")", "input": 3, "tags": {"x": 4}})
                assert not preview.isError and preview.structuredContent["result"] == 10, preview
                assert len((await session.list_resources()).resources) == 3
                await session.list_resource_templates()
                await session.read_resource("opcmodbus://examples")
                assert not (await session.call_tool("server_stop", {"confirm": True})).isError
                assert not (await tool_status(session))["running"]
                patched = await session.call_tool("patch_config", {"expected_revision": config["revision"], "patch": {"poll_groups": [{"id": "fast", "period_ms": 200}]}})
                assert not patched.isError, patched
                info = (await session.call_tool("connection_info", {})).structuredContent
                info_matches(info, server, path)
                owner = endpoint(path)
                async with httpx.AsyncClient(headers={"Authorization": "Bearer " + owner["token"]}, trust_env=False) as client:
                    async with streamable_http_client(owner["url"], http_client=client) as (hr, hw, _):
                        async with ClientSession(hr, hw, client_info=types.Implementation(name="official-http-test", version="1")) as http:
                            await http.initialize()
                            status = await tool_status(http)
                            assert status["instance_id"] == owner["instance_id"]
                            assert not status["running"]
                            async with AsyncExitStack() as attached_stack:
                                attached, _ = await open_client(attached_stack, server, path, "attached-bridge")
                                other = await wait_for(lambda: tool_status(attached), lambda s: len(s["mcp_clients"]) == 3,
                                    label="stdio and HTTP registrations", processes=processes)
                                assert other["instance_id"] == status["instance_id"]
                                assert client_names(other) == ["attached-bridge", "official-http-test", "official-sdk-main"], other
                                assert sorted(c["transport"] for c in other["mcp_clients"]) == ["HTTP", "stdio", "stdio"]
                                assert not (await attached.call_tool("server_start", {})).isError
                            status = await wait_for(lambda: tool_status(http), lambda s: len(s["mcp_clients"]) == 2,
                                label="attached bridge EOF", processes=processes)
                            assert status["running"]
                            assert not (await http.call_tool("server_stop", {"confirm": True})).isError
                            assert not (await tool_status(http))["running"]
                await wait_for(lambda: tool_status(session), lambda s: client_names(s) == ["official-sdk-main"],
                    label="HTTP disconnect", processes=processes)
                assert console.returncode is None
                assert not (await session.call_tool("server_start", {})).isError
                assert (await tool_status(session))["running"]
                assert not (await session.call_tool("shutdown_controller", {"confirm": True})).isError
                assert await asyncio.wait_for(console.wait(), 10) == 0
                assert not Path(str(path) + ".mcp-endpoint.json").exists()
                await offline_status(session, path)
                await session.send_ping()
                assert len((await session.list_tools()).tools) == len(tools.tools)
                await ensure_no_hidden_host(path)
            # A new Codex/Claude bridge after explicit shutdown also waits.
            async with AsyncExitStack() as stack:
                session, _ = await open_client(stack, server, path, "after-shutdown")
                await offline_status(session, path)
                assert not Path(str(path) + ".mcp-endpoint.json").exists()
            print(f"PASS: {len(tools.tools)} tools; six concurrent offline/attached clients; official SDK stdio + HTTP; config, formulas, disconnect, console shutdown; no automatic host", flush=True)
        finally:
            await stop_processes(processes)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, default=Path("target/debug/opc-modbus-server.exe"))
    parser.add_argument("--sdk-dir", type=Path, default=Path("target/mcp-sdk-test"))
    args = parser.parse_args()
    bootstrap_sdk(args.sdk_dir)
    asyncio.run(exercise(args.exe.resolve()))
