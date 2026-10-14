"""Check actual EXEs: client-first startup, exact targeting and owner restart.

The MCP process is a stdio bridge started by Codex/Claude. It must initialize
while its selected configuration has no GUI/server owner, without creating a
hidden controller. This script starts and stops only its own isolated instances.

python scripts/check-mcp-startup.py --server dist/windows/opc-modbus-server.exe \
    --gui dist/windows/opc-modbus-config.exe
"""
import argparse
import asyncio
from contextlib import AsyncExitStack
import itertools
from pathlib import Path
import tempfile

from mcp_test_support import (
    bootstrap_sdk, client_names, endpoint, ensure_no_hidden_host, info_matches,
    offline_status, open_client, raw_status, spawn_owner, stop_processes,
    tool_status, wait_for, write_config,
)


async def startup_order(server, gui, directory, order):
    path = write_config(directory)
    processes = []
    first_owner = None
    first_instance = None
    session = None
    label = " -> ".join(order)
    async with AsyncExitStack() as clients:
        try:
            for item in order:
                if item == "client":
                    session, init = await open_client(clients, server, path, "startup-order-client")
                    assert init.serverInfo.name
                    assert len((await session.list_tools()).tools) >= 30
                    assert len((await session.list_resources()).resources) == 3
                    await session.send_ping()
                    if first_owner is None:
                        await offline_status(session, path)
                        assert not Path(str(path) + ".mcp-endpoint.json").exists(), label
                        await ensure_no_hidden_host(path)
                else:
                    process = await spawn_owner(item, server, gui, path)
                    processes.append(process)
                    if first_owner is None:
                        first_owner = (item, process)
                    status = await wait_for(
                        lambda: raw_status(path),
                        lambda status: status["running"] if "server" in order[:order.index(item) + 1] else True,
                        label=label,
                        processes=processes,
                    )
                    owner = endpoint(path)
                    # GUI-first really means an inline GUI controller, never a
                    # hidden server process. A later console attaches to it.
                    assert owner["pid"] == first_owner[1].pid, (label, owner)
                    if first_instance is None:
                        first_instance = status["instance_id"]
                    assert status["instance_id"] == first_instance, label
                if session is not None and first_owner is not None:
                    await wait_for(
                        lambda: raw_status(path),
                        lambda status: client_names(status) == ["startup-order-client"],
                        label=label,
                        processes=processes,
                    )
            status = await wait_for(
                lambda: tool_status(session),
                lambda status: status.get("attached") is True and status["running"],
                label=label,
                processes=processes,
            )
            assert status["instance_id"] == first_instance, label
            assert client_names(status) == ["startup-order-client"], status
            client = status["mcp_clients"][0]
            assert client["transport"] == "stdio" and client["version"] == "1", client
            identity = (status["config_id"], status["instance_name"])
            info = (await session.call_tool("connection_info", {})).structuredContent
            info_matches(info, server, path)

            # Closing the original GUI/console must leave the other launched
            # owner available. It takes over the same config and preserves the
            # active polling intent while the existing stdio session reconnects.
            survivor = next(process for process in processes if process.pid != first_owner[1].pid)
            await stop_processes([first_owner[1]])
            handed_off = await wait_for(
                lambda: tool_status(session),
                lambda current: current.get("attached") is True
                    and current.get("instance_id") != first_instance
                    and current["running"]
                    and client_names(current) == ["startup-order-client"],
                label=label + " controller handoff",
                processes=[survivor],
            )
            assert endpoint(path)["pid"] == survivor.pid, handed_off
            assert (handed_off["config_id"], handed_off["instance_name"]) == identity
            first_instance = handed_off["instance_id"]
            await ensure_no_hidden_host(path)

            # Remove every potential GUI/server owner. Keep the same stdio
            # session alive and prove it waits without launching mcp-host.
            await stop_processes(processes)
            await offline_status(session, path)
            await asyncio.sleep(1.3)  # Allow at least one bridge heartbeat.
            await offline_status(session, path)
            await ensure_no_hidden_host(path)

            replacement = await spawn_owner(first_owner[0], server, gui, path)
            processes.append(replacement)
            recovered = await wait_for(
                lambda: tool_status(session),
                lambda status: status.get("attached") is True
                    and status.get("instance_id") != first_instance
                    and client_names(status) == ["startup-order-client"],
                label=label + " owner restart",
                processes=[replacement],
            )
            assert endpoint(path)["pid"] == replacement.pid, recovered
            assert (recovered["config_id"], recovered["instance_name"]) == identity
            started = await session.call_tool("server_start", {})
            assert not started.isError, started
            assert (await tool_status(session))["running"]
            await clients.aclose()
            await wait_for(lambda: raw_status(path), lambda status: not status["mcp_clients"], label=label + " disconnect")
            print("PASS:", label, "; correct owner, running handoff, offline bridge, owner restart, disconnect", flush=True)
        finally:
            await stop_processes(processes)


async def exact_targeting(server, gui, root):
    # Identical filenames/configuration content in different directories must
    # remain separate targets even while both GUI windows are open.
    path_a = write_config(root / "instance-a")
    path_b = write_config(root / "instance-b")
    processes = []
    # Enter B before A so A can be closed first without violating the SDK's
    # nested AnyIO cancellation scopes in this task.
    async with AsyncExitStack() as client_b, AsyncExitStack() as client_a:
        try:
            b, _ = await open_client(client_b, server, path_b, "Claude-test")
            a, _ = await open_client(client_a, server, path_a, "Codex-test")
            await offline_status(a, path_a)
            await offline_status(b, path_b)
            owner_a = await spawn_owner("gui", server, gui, path_a)
            owner_b = await spawn_owner("gui", server, gui, path_b)
            processes.extend([owner_a, owner_b])
            status_a = await wait_for(lambda: tool_status(a), lambda s: client_names(s) == ["Codex-test"], label="target A", processes=processes)
            status_b = await wait_for(lambda: tool_status(b), lambda s: client_names(s) == ["Claude-test"], label="target B", processes=processes)
            assert status_a["config_id"] != status_b["config_id"]
            assert status_a["instance_id"] != status_b["instance_id"]
            assert endpoint(path_a)["pid"] == owner_a.pid
            assert endpoint(path_b)["pid"] == owner_b.pid
            info_a = (await a.call_tool("connection_info", {})).structuredContent
            info_b = (await b.call_tool("connection_info", {})).structuredContent
            info_matches(info_a, server, path_a)
            info_matches(info_b, server, path_b)
            assert info_a["codex_toml"].splitlines()[0] != info_b["codex_toml"].splitlines()[0], (info_a, info_b)
            assert not status_a["running"] and not status_b["running"]
            assert not (await a.call_tool("server_start", {})).isError
            assert (await tool_status(a))["running"]
            assert not (await tool_status(b))["running"]

            await stop_processes([owner_a])
            await offline_status(a, path_a)
            assert client_names(await tool_status(b)) == ["Claude-test"]
            await asyncio.sleep(1.3)
            await offline_status(a, path_a)
            await ensure_no_hidden_host(path_a)
            replacement = await spawn_owner("gui", server, gui, path_a)
            processes.append(replacement)
            recovered = await wait_for(lambda: tool_status(a), lambda s: client_names(s) == ["Codex-test"], label="target A reconnect", processes=[replacement])
            assert recovered["config_id"] == status_a["config_id"]
            assert recovered["instance_id"] != status_a["instance_id"]
            assert client_names(await tool_status(b)) == ["Claude-test"]
            await client_a.aclose()
            await wait_for(lambda: raw_status(path_a), lambda s: not s["mcp_clients"], label="target A disconnect")
            assert client_names(await tool_status(b)) == ["Claude-test"]
            # Explicit shutdown must win over both automatic takeover loops.
            # The second config remains usable, proving the stop is scoped.
            console_b = await spawn_owner("server", server, gui, path_b)
            processes.append(console_b)
            await wait_for(lambda: tool_status(b), lambda s: s["running"], label="target B console attach", processes=[owner_b, console_b])
            assert endpoint(path_b)["pid"] == owner_b.pid
            shutdown = await b.call_tool("shutdown_controller", {"confirm": True})
            assert not shutdown.isError, shutdown
            assert await asyncio.wait_for(console_b.wait(), 10) == 0
            await offline_status(b, path_b)
            await asyncio.sleep(1.3)
            await offline_status(b, path_b)
            assert not Path(str(path_b) + ".mcp-endpoint.json").exists(), "explicitly stopped controller was recreated"
            await ensure_no_hidden_host(path_b)
            assert not (await raw_status(path_a))["mcp_clients"]
            await client_b.aclose()
            print("PASS: two simultaneous GUI instances; exact Codex/Claude targets, independent runtime, restart, disconnect and scoped shutdown", flush=True)
        finally:
            await stop_processes(processes)


async def stopped_handoff(server, gui, root):
    """Owner recovery preserves an explicit server_stop, including its race."""
    for first, second in (("gui", "server"), ("server", "gui")):
        path = write_config(root / f"{first}-to-{second}")
        processes = []
        async with AsyncExitStack() as clients:
            try:
                primary = await spawn_owner(first, server, gui, path)
                processes.append(primary)
                await wait_for(lambda: raw_status(path), lambda s: s["running"] if first == "server" else True,
                    label="stopped handoff owner", processes=processes)
                if second == "server":
                    survivor = await spawn_owner(second, server, gui, path)
                    processes.append(survivor)
                    await wait_for(lambda: raw_status(path), lambda s: s["running"], label="stopped handoff console attach", processes=processes)
                session, _ = await open_client(clients, server, path, "stopped-handoff-client")
                previous = await tool_status(session)
                assert previous["running"]
                stopped = await session.call_tool("server_stop", {"confirm": True})
                assert not stopped.isError, stopped
                if second == "gui":
                    survivor = await spawn_owner(second, server, gui, path)
                    processes.append(survivor)
                # No settling delay: a just-stopped owner may die before a
                # monitor takes another sample of the desired runtime state.
                await stop_processes([primary])
                recovered = await wait_for(lambda: tool_status(session),
                    lambda s: s.get("attached") is True and s.get("instance_id") != previous["instance_id"]
                        and client_names(s) == ["stopped-handoff-client"],
                    label=f"explicit stop then {first}-to-{second} handoff", processes=[survivor])
                assert endpoint(path)["pid"] == survivor.pid, recovered
                assert not recovered["running"], "owner takeover restarted explicitly stopped polling"
                await asyncio.sleep(1.3)
                assert not (await tool_status(session))["running"], "polling resumed after explicit stop"
                await clients.aclose()
                await wait_for(lambda: raw_status(path), lambda s: not s["mcp_clients"], label="stopped handoff disconnect")
                print(f"PASS: explicit server_stop -> {first}-to-{second} ownership handoff preserves idle state", flush=True)
            finally:
                await stop_processes(processes)


async def main(args):
    server, gui = args.server.resolve(), args.gui.resolve()
    assert server.is_file() and gui.is_file(), "build the selected EXEs first"
    with tempfile.TemporaryDirectory(prefix="opc-mcp-startup-") as directory:
        root = Path(directory)
        orders = list(itertools.permutations(("gui", "client", "server")))
        if args.order is not None:
            orders = [] if args.order in ("targeting", "stopped-handoff") else [tuple(args.order.split("-"))]
        for order in orders:
            await startup_order(server, gui, root / "-".join(order), order)
        if args.order is None or args.order == "targeting":
            await exact_targeting(server, gui, root / "targeting")
        if args.order is None or args.order == "stopped-handoff":
            await stopped_handoff(server, gui, root / "stopped-handoff")
    checks = "all six startup orders, exact instance targeting and explicit-stop handoffs" if args.order is None else args.order
    print(f"PASS: {checks} on actual Windows GUI/server EXEs", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--gui", type=Path, required=True)
    parser.add_argument("--sdk-dir", type=Path, default=Path("target/mcp-sdk-test"))
    parser.add_argument("--order", choices=["-".join(order) for order in itertools.permutations(("gui", "client", "server"))] + ["targeting", "stopped-handoff"], help="run only one scenario while diagnosing a failure")
    args = parser.parse_args()
    bootstrap_sdk(args.sdk_dir)
    asyncio.run(main(args))
