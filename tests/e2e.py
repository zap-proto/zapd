#!/usr/bin/env python3
"""End-to-end test for the zapd universal router (ZAP router envelope, binary)."""
import socket, struct, sys, time

HELLO,WELCOME,PROVIDERS_LIST,PROVIDERS,PEER_CONNECTED,PEER_DISCONNECTED,ERROR=1,2,3,4,5,6,7
ROUTE,RESPONSE,EVENT=16,17,18
ROLE_PROVIDER,ROLE_CONSUMER=1,2
SOCK=sys.argv[1] if len(sys.argv)>1 else "/tmp/zapd-e2e.sock"

def put_str(s):
    b=s.encode(); return struct.pack('<H',len(b))+b

def encode(typ,frm,to,payload=b'',flags=0):
    fb,tb=frm.encode(),to.encode()
    body=struct.pack('<BHHHI',typ,flags,len(fb),len(tb),len(payload))+fb+tb+payload
    return struct.pack('<I',len(body))+body

def recvn(s,n):
    buf=b''
    while len(buf)<n:
        c=s.recv(n-len(buf))
        if not c: raise EOFError("closed")
        buf+=c
    return buf

def read_frame(s):
    (length,)=struct.unpack('<I',recvn(s,4))
    buf=recvn(s,length)
    typ,flags,fl,tl,pl=struct.unpack_from('<BHHHI',buf,0)
    o=11
    frm=buf[o:o+fl].decode(); o+=fl
    to=buf[o:o+tl].decode(); o+=tl
    pay=buf[o:o+pl]
    return typ,frm,to,pay

def read_until(s,want):
    for _ in range(20):
        typ,frm,to,pay=read_frame(s)
        if typ==want: return typ,frm,to,pay
    raise AssertionError(f"never got type {want}")

def hello_payload(role,brand,caps):
    b=bytes([role])+put_str(brand)+struct.pack('<H',len(caps))
    for c in caps: b+=put_str(c)
    return b

def parse_providers(pay):
    (n,)=struct.unpack_from('<H',pay,0); o=2; out=[]
    for _ in range(n):
        (idl,)=struct.unpack_from('<H',pay,o); o+=2
        pid=pay[o:o+idl].decode(); o+=idl
        role=pay[o]; o+=1
        (bl,)=struct.unpack_from('<H',pay,o); o+=2
        brand=pay[o:o+bl].decode(); o+=bl
        (cn,)=struct.unpack_from('<H',pay,o); o+=2
        caps=[]
        for _ in range(cn):
            (cl,)=struct.unpack_from('<H',pay,o); o+=2
            caps.append(pay[o:o+cl].decode()); o+=cl
        out.append((pid,role,brand,caps))
    return out

def conn():
    s=socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(SOCK); return s

# 1) provider connects
prov=conn()
prov.sendall(encode(HELLO,"browser:chrome/default","",hello_payload(ROLE_PROVIDER,"hanzo",["browser.tabs","browser.navigate"])))
assert read_until(prov,WELCOME)[1]=="zapd"
print("provider: registered + WELCOME ✓")

# 2) consumer connects
cons=conn()
cons.sendall(encode(HELLO,"consumer:hanzo-mcp/claude","",hello_payload(ROLE_CONSUMER,"hanzo",[])))
assert read_until(cons,WELCOME)
print("consumer: registered + WELCOME ✓")

# 3) consumer lists providers
cons.sendall(encode(PROVIDERS_LIST,"consumer:hanzo-mcp/claude",""))
_,_,_,pay=read_until(cons,PROVIDERS)
provs=parse_providers(pay)
assert any(p[0]=="browser:chrome/default" and p[2]=="hanzo" for p in provs), provs
print(f"consumer: providers.list -> {[p[0] for p in provs]} ✓")

# 4) consumer routes an OPAQUE frame to the provider
OPAQUE_REQ=b"\x00\x01\x02 zap-payload: tabs.list (opaque to router)"
cons.sendall(encode(ROUTE,"consumer:hanzo-mcp/claude","browser:chrome/default",OPAQUE_REQ))
typ,frm,to,pay=read_until(prov,ROUTE)
assert frm=="consumer:hanzo-mcp/claude", f"from not stamped: {frm}"   # router stamped verified id
assert to=="browser:chrome/default"
assert pay==OPAQUE_REQ, "payload mutated!"   # opaque, byte-identical
print(f"provider: received ROUTE from='{frm}' payload intact ({len(pay)}B) ✓")

# 5) provider responds; consumer receives, from stamped
OPAQUE_RESP=b"RESULT:[tab1,tab2] (opaque)"
prov.sendall(encode(RESPONSE,"browser:chrome/default","consumer:hanzo-mcp/claude",OPAQUE_RESP))
typ,frm,to,pay=read_until(cons,RESPONSE)
assert frm=="browser:chrome/default" and pay==OPAQUE_RESP
print(f"consumer: received RESPONSE from='{frm}' payload intact ✓")

# 6) presence: provider sees consumer disconnect
cons.close()
typ,frm,to,pay=read_until(prov,PEER_DISCONNECTED)
(idl,)=struct.unpack_from('<H',pay,0)
gone=pay[2:2+idl].decode()
assert gone=="consumer:hanzo-mcp/claude", gone
print(f"provider: PEER_DISCONNECTED '{gone}' ✓")

print("\nALL E2E ROUTER CHECKS PASSED ✓")
