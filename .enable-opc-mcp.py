from pathlib import Path
import re
p=Path(r'C:\Users\gorelyi\.codex\config.toml')
data=p.read_bytes()
text=data.decode('utf-8-sig')
match=re.search(r'(?ms)^\[mcp_servers\.opc_modbus\]\r?\n.*?(?=^\[|\Z)',text)
assert match, 'Existing OPC Modbus MCP entry not found'
section=match.group(0)
assert 'dist\\windows\\opc-modbus-server.exe' in section
assert 'C:\\ProgramData\\OPC Modbus Server\\config.json' in section
updated,count=re.subn(r'(?m)^enabled\s*=\s*false\s*$', 'enabled = true',section,count=1)
assert count==1, 'Entry is not explicitly disabled as expected'
result=text[:match.start()]+updated+text[match.end():]
p.with_name('config.toml.before-opc-mcp-enable-20261014').write_bytes(data)
p.write_bytes((b'\xef\xbb\xbf' if data.startswith(b'\xef\xbb\xbf') else b'')+result.encode('utf-8'))
print('opc_modbus enabled=true; executable and selected config path preserved')
