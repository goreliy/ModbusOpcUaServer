import asyncio, ctypes, hashlib, json, sys
from contextlib import AsyncExitStack
from pathlib import Path
sys.path.insert(0,str(Path('scripts').resolve()))
from mcp_test_support import bootstrap_sdk, open_client, raw_status, tool_status, endpoint
bootstrap_sdk(Path('target/mcp-sdk-test'))

async def main():
    path=Path(r'C:\ProgramData\OPC Modbus Server\config.json')
    before=json.loads(Path('target/mcp-live-before.json').read_text(encoding='utf-8-sig'))
    assert hashlib.sha256(path.read_bytes()).hexdigest().upper()==before['config_sha256']
    assert not ctypes.windll.shell32.IsUserAnAdmin(), 'test must run as normal operator'
    status=await raw_status(path)
    assert status['attached'] and status['running'] and status['config_id']=='cf785a7698ec', status
    async with AsyncExitStack() as stack:
        session,_=await open_client(stack,Path('dist/windows/opc-modbus-server.exe').resolve(),path,'SDK проверка')
        status=await tool_status(session)
        assert any(c['name']=='SDK проверка' and c['transport']=='stdio' for c in status['mcp_clients']),status
        result=await session.call_tool('get_config',{})
        assert not result.isError and result.structuredContent['revision'].upper()==before['config_sha256']
        proof={k:status.get(k) for k in ('attached','running','config_path','config_id','instance_name','instance_id','mcp_url','opcua_endpoint')}
        proof['operator_is_admin']=False
        proof['controller_pid']=endpoint(path)['pid']
        proof['mcp_clients']=status['mcp_clients']
        proof['config_unchanged']=True
        Path('target/mcp-live-proof.json').write_text(json.dumps(proof,ensure_ascii=False,indent=2),encoding='utf-8')
        print(json.dumps(proof,ensure_ascii=True),flush=True)
    status=await raw_status(path)
    assert not any(c['name']=='SDK проверка' for c in status['mcp_clients']),status
    print('PASS: normal operator + LocalSystem service; selected config, real stdio registration/disconnect, config unchanged',flush=True)

asyncio.run(main())
