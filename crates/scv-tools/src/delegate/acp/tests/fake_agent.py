import json, os, sys
DIR, MODE = sys.argv[1], sys.argv[2]
open(os.path.join(DIR, "pid"), "w").write(str(os.getpid()))
def log(line):
    with open(os.path.join(DIR, "calls"), "a") as f:
        f.write(line + "\n")
def send(message):
    message["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()
def update(session, body):
    send({"method": "session/update", "params": {"sessionId": session, "update": body}})
def chunk(session, text):
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
OPTIONS = [{"id": "model", "options": [{"value": "m1"}, {"value": "m2"}]},
           {"id": "reasoning_effort", "options": [{"value": "low"}, {"value": "high"}]}]
memory, pending, refusals, sessions = "", None, [], 0
for line in sys.stdin:
    message = json.loads(line)
    method, params, rid = message.get("method"), message.get("params") or {}, message.get("id")
    if method is None:
        if rid == 900 and pending:
            outcome = message["result"]["outcome"]
            chunk(pending[1], "chose " + outcome.get("optionId", outcome["outcome"]))
            send({"id": pending[0], "result": {"stopReason": "end_turn"}})
            pending = None
        elif rid in (901, 902) and pending:
            refusals.append(str(message.get("error", {}).get("code")))
            if len(refusals) == 2:
                chunk(pending[1], "refused:" + ",".join(refusals))
                send({"id": pending[0], "result": {"stopReason": "end_turn"}})
                pending = None
        continue
    if method == "initialize":
        json.dump(params, open(os.path.join(DIR, "init.json"), "w"))
        send({"id": rid, "result": {"protocolVersion": 2 if MODE == "v2" else 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        sessions += 1
        log("session/new cwd=" + params["cwd"])
        if MODE == "auth":
            send({"id": rid, "error": {"code": -32000, "message": "Authentication required"}})
            continue
        send({"id": rid, "result": {"sessionId": "s%d" % sessions,
              "modes": {"currentModeId": "default", "availableModes": [{"id": "default", "name": "Default"}, {"id": "bypassPermissions", "name": "Bypass"}]},
              "configOptions": OPTIONS}})
    elif method == "session/set_mode":
        log("mode=" + params["modeId"])
        send({"id": rid, "result": {}})
    elif method == "session/set_config_option":
        log(params["configId"] + "=" + params["value"])
        send({"id": rid, "result": {"configOptions": OPTIONS}})
    elif method == "session/cancel":
        log("cancel")
        if pending and MODE != "stubborn":
            send({"id": pending[0], "result": {"stopReason": "cancelled"}})
            pending = None
    elif method == "session/prompt":
        session, text = params["sessionId"], params["prompt"][0]["text"]
        if text.startswith("remember "):
            memory = text.split(" ", 1)[1]
            chunk(session, "noted\n")
            update(session, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read notes.txt", "kind": "read", "status": "pending"})
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "PRIVATE-OUTPUT"}}]})
            update(session, {"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "curl -H 'Authorization: Bearer sk-live-123456' x", "kind": "execute", "status": "pending"})
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "failed"})
            update(session, {"sessionUpdate": "plan", "entries": [{"content": "store the word", "status": "in_progress", "priority": "high"}]})
            update(session, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "PRIVATE-THOUGHT"}})
            chunk(session, "stored " + memory)
            send({"id": rid, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 3, "outputTokens": 4}}})
        elif text == "recall":
            chunk(session, "the word is " + memory + " in " + session)
            send({"id": rid, "result": {"stopReason": "end_turn"}})
        elif text == "permission":
            pending = (rid, session)
            send({"id": 900, "method": "session/request_permission", "params": {"sessionId": session,
                  "toolCall": {"toolCallId": "t3", "title": "Write /tmp/scv-acp.txt", "kind": "edit", "locations": [{"path": "/tmp/scv-acp.txt"}]},
                  "options": [{"optionId": "allow-once", "name": "Allow", "kind": "allow_once"},
                              {"optionId": "always", "name": "Always", "kind": "allow_always"},
                              {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
        elif text == "client":
            pending, refusals = (rid, session), []
            send({"id": 901, "method": "fs/read_text_file", "params": {"sessionId": session, "path": "/etc/hosts"}})
            send({"id": 902, "method": "terminal/create", "params": {"sessionId": session, "command": "ls"}})
        elif text == "hang":
            log("hang")
            pending = (rid, session)
        elif text == "die":
            sys.stderr.write("boom: out of memory\n"); sys.stderr.flush()
            sys.exit(3)
        elif text == "fail":
            send({"id": rid, "error": {"code": -32603, "message": "Internal error",
                  "data": {"message": "Unauthorized (401): Bearer sk-live-abcdef123456 expired"}}})
        elif text == "refuse":
            chunk(session, "I can't help get past authentication or a 403 on that site.")
            send({"id": rid, "result": {"stopReason": "refusal"}})
        elif text == "env":
            chunk(session, "CODEX_CONFIG=" + os.environ.get("CODEX_CONFIG", "unset"))
            send({"id": rid, "result": {"stopReason": "end_turn"}})
        else:
            chunk(session, "echo " + text)
            send({"id": rid, "result": {"stopReason": "end_turn"}})
    elif rid is not None:
        send({"id": rid, "error": {"code": -32601, "message": "Method not found"}})
