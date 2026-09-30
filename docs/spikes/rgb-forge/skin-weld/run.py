#!/usr/bin/env python3
import json, sys, time, struct, urllib.request, pathlib
BASE="http://127.0.0.1:8850"; TOKEN="replbench"; D=pathlib.Path(__file__).parent / "out"
def http(method, path, body=None, ctype="application/json"):
    r=urllib.request.Request(BASE+path, data=body, method=method, headers={"Authorization":"Bearer "+TOKEN, "Content-Type":ctype})
    try: return urllib.request.urlopen(r, timeout=1800).read()
    except urllib.error.HTTPError as e: return e.read()
def cmd(c,a): return json.loads(http("POST","/v1/command",json.dumps({"command":c,"args":a}).encode()))
def put(path):
    data=(D/path).read_bytes(); t=time.time()
    r=json.loads(http("POST","/v1/blob",data,"application/octet-stream"))
    return {"handle":r["handle"],"len":r["len"],"ms":(time.time()-t)*1000}
def ref(b): return [b["handle"], b["len"]]
def unpack(data, magic):
    m,n=struct.unpack_from("<II",data,0); assert m==magic,(hex(m),hex(magic))
    lens=struct.unpack_from("<"+"Q"*n,data,8); at=8+8*n; out=[]
    for l in lens: out.append(data[at:at+l]); at+=(l+7)//8*8
    return out
head=put("head.skin"); body=put("body.skin")
print("uploaded head", head["len"], "B, body", body["len"], "B in %.1f + %.1f ms"%(head["ms"],body["ms"]))
params=json.loads((D/"params.json").read_text())
source=(D/"guest.rs").read_text()
manifest='[package]\nname = "cell"\nversion = "0.1.0"\nedition = "2024"\n[dependencies]\nglam = "=0.33.10"\n'
lock='version = 4\n\n[[package]]\nname = "glam"\nversion = "0.33.10"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "928452f9c953e142b2f0973e4bd5f34445fcaa8069556ea97a3c9d34d15a4cf8"\n'
optimize = "--slow" not in sys.argv
t=time.time()
e=cmd("eval",{"source":source,"manifest":manifest,"lock":lock,"args":[ref(head),ref(body),params],"optimize":optimize})
wall=(time.time()-t)*1000
if not e["ok"]:
    print("EVAL FAILED", wall, json.dumps(e["result"])[:1500]); print(json.dumps(e.get("diagnostics"))[:2500]); sys.exit(1)
res=e["result"]; out=res["output"]
print("eval ok: wall %.0f ms, build %s ms, run %.2f ms, output %s"%(wall,res["build"]["ms"],res["runtime_ms"]["run_ms"],str(out)[:140]))
if isinstance(out,dict) and "Err" in out: print("guest error:", out); sys.exit(2)
ok = out["Ok"] if isinstance(out,dict) else out
handle=ok[0]; length=ok[1]
blob=http("GET","/v1/blob/"+handle); assert len(blob)==length,(len(blob),length)
expected=(D/"expected.weld").read_bytes()
WELD=0x31444c57; SKIN=0x314e4b53
g=unpack(blob,WELD); x=unpack(expected,WELD)
import array
def floats(b,code): a=array.array(code); a.frombytes(b); return list(a)
report=floats(g[6],"d"); xreport=floats(x[6],"d")
print("report native  :", [round(v,6) for v in xreport]); print("report loom    :", [round(v,6) for v in report])
names=["positions","normals","uvs","joints","weights","triangles"]
worst=0; exact=True
for which,label in ((0,"head"),(1,"body")):
    gs=unpack(g[which],SKIN); xs=unpack(x[which],SKIN)
    assert len(gs)==len(xs),(label,len(gs),len(xs))
    for i,(a,b) in enumerate(zip(gs,xs)):
        nm=names[i] if i<6 else "target%d"%(i-6)
        if a==b: continue
        exact=False
        if nm in ("positions","normals","uvs","weights") or nm.startswith("target"):
            fa,fb=floats(a,"f"),floats(b,"f"); assert len(fa)==len(fb),(label,nm); d=max(abs(p-q) for p,q in zip(fa,fb)); worst=max(worst,d)
            print("  %s %s differs by at most %.2e (%d floats)"%(label,nm,d,len(fa)))
        else:
            print("  %s %s DIFFERENT (%d vs %d bytes)"%(label,nm,len(a),len(b))); worst=float("inf")
for i in (2,3,4,5):
    if g[i]!=x[i]: print("  index array",i,"DIFFERENT"); worst=float("inf"); exact=False
print("RESULT:", "bit-exact" if exact and report==xreport else ("within %.1e (report equal: %s)"%(worst, report==xreport)))
