"""End-to-end CLI/HTTP tests against explicit local model and KEI fixtures.

Run after cargo build -p aris-cli --bin aris and npm run build in web/.
All generated data stays in a temporary directory outside the repository.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit, urlencode
from urllib.request import Request, urlopen
import base64
from concurrent.futures import ThreadPoolExecutor

ROOT = Path(__file__).resolve().parents[1]
def request(url, value=None, headers=None):
    raw = None if value is None else json.dumps(value).encode()
    req = Request(url, data=raw, headers={"Content-Type": "application/json", **(headers or {})})
    with urlopen(req, timeout=30) as response:
        body=response.read()
        return json.loads(body) if response.headers.get_content_type()=="application/json" else body.decode()

class Fixture(BaseHTTPRequestHandler):
    active = 0
    peak = 0
    calls = []
    lock = threading.Lock()
    redemption = None
    def log_message(self, *_): pass
    def respond(self, value, code=200):
        body=json.dumps(value).encode(); self.send_response(code)
        self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        self.respond({"data":[{"id":"fixture-a"},{"id":"fixture-b"}]})
    def do_POST(self):
        data=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path=="/api/aris/import/redeem":
            expected=type(self).redemption
            digest=base64.urlsafe_b64encode(hashlib.sha256(data["code_verifier"].encode()).digest()).decode().rstrip("=")
            if not expected or data["code"]!="fixture-once" or digest!=expected["code_challenge"] or data["redirect_uri"]!=expected["redirect_uri"]:
                self.respond({"success":False},400); return
            type(self).redemption=None
            self.respond({"success":True,"data":{"base_url":f"http://127.0.0.1:{self.server.server_port}/v1","api_key":"fixture-import-only","models":[{"id":"fixture-a","family":"gpt","protocol":"chat_completions","vision":False}]}})
            return
        with self.lock:
            type(self).active+=1; type(self).peak=max(self.peak,self.active); self.calls.append(data)
        if "STREAM-TEST" in json.dumps(data.get("messages",[])):
            self.send_response(200); self.send_header("Content-Type","text/event-stream"); self.end_headers()
            for piece in ("LIVE ", "PARTIAL ", "REPLY"):
                event={"choices":[{"index":0,"delta":{"content":piece},"finish_reason":None}]}
                self.wfile.write(("data: "+json.dumps(event)+"\n\n").encode()); self.wfile.flush(); time.sleep(.3)
            self.wfile.write(b'data: [DONE]\n\n'); self.wfile.flush()
            with self.lock: type(self).active-=1
            return
        time.sleep(0.25)
        with self.lock: type(self).active-=1
        model=data["model"]
        self.respond({"choices":[{"index":0,"message":{"role":"assistant","content":f"VERIFIED-{model}"},"finish_reason":"stop"}]})

class ArisIntegration(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp=tempfile.TemporaryDirectory(prefix="aris-integration-")
        cls.directory=Path(cls.temp.name); cls.workspace=cls.directory/"workspace"; cls.workspace.mkdir()
        cls.fixture=ThreadingHTTPServer(("127.0.0.1",0),Fixture)
        threading.Thread(target=cls.fixture.serve_forever,daemon=True).start()
        cls.gateway=f"http://127.0.0.1:{cls.fixture.server_port}/v1"
        import socket
        sock=socket.socket(); sock.bind(("127.0.0.1",0)); port=sock.getsockname()[1]; sock.close()
        cls.base=f"http://127.0.0.1:{port}"
        binary=Path(os.environ.get("ARIS_TEST_BIN",ROOT/"target/debug"/("aris.exe" if os.name=="nt" else "aris")))
        env=dict(os.environ); env["ARIS_HOME"]=str(cls.directory/"home")
        for name in ("KEI_API_KEY","ARIS_API_KEY","KEI_BASE_URL","ARIS_BASE_URL","ARIS_LLM_BASE_URL","ARIS_MODEL","KEI_MODEL"):
            env.pop(name,None)
        cls.log=open(cls.directory/"server.log","w",encoding="utf-8")
        cls.process=subprocess.Popen([str(binary),"--workspace",str(cls.workspace),"web","--port",str(port),"--no-open","--assets",str(ROOT/"web/dist")],env=env,stdout=cls.log,stderr=cls.log,creationflags=subprocess.CREATE_NO_WINDOW if os.name=="nt" else 0)
        for _ in range(100):
            try:
                cls.token=request(cls.base+"/api/harness/bootstrap")["token"]; break
            except Exception: time.sleep(0.1)
        else: raise RuntimeError("ARIS server did not start")
    @classmethod
    def tearDownClass(cls):
        cls.process.terminate(); cls.process.wait(timeout=15); cls.log.close()
        cls.fixture.shutdown(); cls.fixture.server_close(); cls.temp.cleanup()
    def rpc(self,method,params=None):
        reply=request(self.base+"/api/harness/rpc",{"method":method,"params":params or {}},{"x-aris-token":self.token})
        if "error" in reply: raise RuntimeError(reply["error"])
        return reply.get("result",reply)
    def configure(self):
        self.rpc("kei/credential",{"api_key":"fixture-local-only"})
        self.rpc("kei/update",{"base_url":self.gateway,"profiles":[{"id":"a","name":"Fixture A","family":"gpt","model":"fixture-a"},{"id":"b","name":"Fixture B","family":"gemini","model":"fixture-b"}]})
    def wait_team(self,team_id):
        for _ in range(200):
            team=self.rpc("teams/get",{"id":team_id})["team"]
            if team["status"] not in ("running","cancelling"): return team
            time.sleep(.05)
        self.fail("workflow timed out")
    def test_parallel_fixed_models_dependencies_and_file_rollback(self):
        self.configure(); Fixture.peak=0; Fixture.calls=[]
        agents=[{"id":"a","name":"A","profile_id":"a","files":["a.md"]},{"id":"b","name":"B","profile_id":"b","files":["b.md"]},{"id":"c","name":"C","profile_id":"a","depends_on":["a","b"],"files":["report.md"]}]
        team=self.rpc("teams/create",{"name":"Integration fixture","goal":"Return the assigned model identity","agents":agents})["team"]
        self.rpc("teams/run",{"id":team["id"]}); done=self.wait_team(team["id"])
        self.assertEqual(done["status"],"completed",done); self.assertGreaterEqual(Fixture.peak,2)
        self.assertIn("VERIFIED-fixture-a",done["agents"][0]["output"])
        self.assertIn("VERIFIED-fixture-b",done["agents"][1]["output"])
        dependent=" ".join(str(m["content"]) for m in Fixture.calls[-1]["messages"])
        self.assertIn("VERIFIED-fixture-a",dependent); self.assertIn("VERIFIED-fixture-b",dependent)
        artifact=self.workspace/"report.md"; artifact.write_text("original",encoding="utf-8")
        snapshot=self.rpc("teams/checkpoint",{"id":team["id"],"label":"verified"})["team"]["checkpoints"][-1]
        artifact.write_text("changed",encoding="utf-8")
        self.rpc("teams/restore",{"id":team["id"],"checkpoint_id":snapshot["id"]})
        self.assertEqual(artifact.read_text(encoding="utf-8"),"original")
        self.rpc("teams/handoff",{"id":team["id"],"from":"a","to":"b","instruction":"Check the previous result"})
        self.assertEqual(self.wait_team(team["id"])["status"],"completed")
        Fixture.calls=[]
        self.rpc("teams/message",{"id":team["id"],"agent_id":"b","message":"TARGETED-CORRECTION: check units"})
        continued=self.wait_team(team["id"])
        self.assertEqual(continued["status"],"completed")
        self.assertEqual([call["model"] for call in Fixture.calls],["fixture-b"])
        self.assertIn("TARGETED-CORRECTION",json.dumps(Fixture.calls[0]["messages"]))
        self.assertEqual([a["session_id"] for a in team["agents"]],[a["session_id"] for a in continued["agents"]])
        self.assertEqual(continued["messages"][-1]["agent_id"],"b")
        Fixture.calls=[]
        self.rpc("teams/message",{"id":team["id"],"message":"GLOBAL-REVISION: include limitations"})
        self.assertEqual(self.wait_team(team["id"])["status"],"completed")
        self.assertEqual(len(Fixture.calls),3)
        self.assertTrue(all("GLOBAL-REVISION" in json.dumps(call["messages"]) for call in Fixture.calls))
        # A live conversation accepts follow-ups, persists them, and continues
        # the same session once the current model/tool wave finishes.
        Fixture.calls=[]
        self.rpc("teams/message",{"id":team["id"],"agent_id":"b","message":"First active instruction"})
        queued=self.rpc("teams/message",{"id":team["id"],"agent_id":"b","message":"QUEUED-FOLLOWUP: explain the evidence"})["team"]
        self.assertTrue(queued["messages"][-1]["queued"])
        final=self.wait_team(team["id"])
        self.assertEqual(final["status"],"completed")
        self.assertFalse(any(message["queued"] for message in final["messages"]))
        self.assertIn("QUEUED-FOLLOWUP",json.dumps(Fixture.calls[-1]["messages"]))
        self.assertEqual(final["agents"][1]["session_id"],team["agents"][1]["session_id"])
        switched=self.rpc("teams/model",{"id":team["id"],"agent_id":"b","profile_id":"a"})["team"]
        self.assertEqual(switched["agents"][1]["profile_id"],"a")
        Fixture.calls=[]
        self.rpc("teams/message",{"id":team["id"],"agent_id":"b","message":"Use the selected model now"})
        self.assertEqual(self.wait_team(team["id"])["status"],"completed")
        self.assertEqual(Fixture.calls[-1]["model"],"fixture-a")
        barrier=threading.Barrier(2)
        def send_concurrently(index):
            barrier.wait(timeout=3)
            return self.rpc("teams/message",{"id":team["id"],"agent_id":"b","message":f"CONCURRENT-{index}"})
        with ThreadPoolExecutor(max_workers=2) as pool:
            replies=list(pool.map(send_concurrently,range(2)))
        self.assertTrue(all(reply["accepted"] for reply in replies))
        final=self.wait_team(team["id"])
        self.assertEqual(final["status"],"completed")
        self.assertTrue(all(any(m["text"]==f"CONCURRENT-{i}" for m in final["messages"]) for i in range(2)))
        self.assertTrue(all(f"CONCURRENT-{i}" in json.dumps(Fixture.calls[-1]["messages"]) for i in range(2)))
    def test_streaming_reply_is_visible_before_turn_finishes(self):
        self.configure()
        team=self.rpc("teams/create",{"name":"Streaming acceptance","goal":"STREAM-TEST","agents":[{"id":"stream","name":"Stream","profile_id":"a"}]})["team"]
        self.rpc("teams/run",{"id":team["id"]})
        observed=False
        for _ in range(100):
            current=self.rpc("teams/get",{"id":team["id"]})["team"]
            if current["agents"][0]["status"]=="running" and current["agents"][0]["output"].startswith("LIVE"):
                observed=True; break
            time.sleep(.03)
        self.assertTrue(observed,"No live reply before completion")
        self.assertEqual(self.wait_team(team["id"])["agents"][0]["output"],"LIVE PARTIAL REPLY")

    def test_templates_and_pkce_import_callback(self):
        self.configure()
        templates=self.rpc("templates/list")["templates"]; self.assertGreaterEqual(len(templates),10)
        source=templates[0]; source["name"]="Custom fixed model"; source["agents"][0]["profile_id"]="b"
        custom=self.rpc("templates/save",{"template":source})["template"]
        self.assertFalse(custom["builtin"]); self.assertEqual(custom["agents"][0]["profile_id"],"b")
        self.rpc("templates/delete",{"id":custom["id"]})
        url=self.rpc("kei/import/start")["url"]
        query={k:v[0] for k,v in parse_qs(urlsplit(url).query).items()}; Fixture.redemption=query
        self.assertNotIn("fixture-local-only",url)
        callback=query["redirect_uri"]+"?"+urlencode({"state":query["state"],"code":"fixture-once"})
        result=request(callback,headers={"Sec-Fetch-Site":"cross-site"}); self.assertIn("模型已导入",result)
        status=self.rpc("kei/import/status"); self.assertEqual(status["status"],"completed")
        imported=self.rpc("kei/config"); self.assertTrue(any(p["model"]=="fixture-a" and (p.get("api_key_env") or "").startswith("ARIS_KEY_") for p in imported["profiles"]))
        self.assertNotIn("fixture-import-only",json.dumps(imported))
        with self.assertRaises(HTTPError): request(callback)
        self.assertIsNone(Fixture.redemption)

if __name__=="__main__": unittest.main(verbosity=2)
