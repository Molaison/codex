"""Official composer predictions only. No ordinary inference fallback.

Executed by the TUI in an isolated Python process. The pinned desktop backend owns
the experimental prediction mode and its HTTP/WebSocket classification. Primary
provider credentials/configuration are never copied into the companion process.
"""
import base64
import ctypes
import datetime
import hashlib
import json
import os
import pathlib
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import uuid

BACKEND_SHA256 = "70ea33b7094a4c20c007d09b83bb50e33debb9bbf99bf066709f9eb85bad524a"
BACKEND_NAME = "codex-desktop-26.1013.3656.0-linux"


def main():
    request = json.loads(sys.stdin.buffer.readline(60000).decode("utf-8"))
    model = request["model"]
    if model not in ("gpt-6.1-sol", "gpt-6-astra"):
        raise RuntimeError("Unsupported prediction model; no fallback")
    backend = pathlib.Path(request["binary_directory"]) / BACKEND_NAME
    digest = hashlib.sha256()
    with backend.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != BACKEND_SHA256:
        raise RuntimeError("Official prediction backend hash mismatch")
    auth = json.loads((pathlib.Path(request["codex_home"]) / "auth.json").read_text(encoding="utf-8"))
    if auth.get("auth_mode") != "chatgpt" or auth.get("OPENAI_API_KEY"):
        raise RuntimeError("ChatGPT file login required; API credentials are not accepted")
    tokens = auth["tokens"]
    claims = json.loads(base64.urlsafe_b64decode(tokens["access_token"].split(".")[1] + "==="))
    if claims.get("exp", 0) <= time.time() + 300:
        raise RuntimeError("ChatGPT login expired or expiring; no refresh or paid fallback")
    if claims.get("https://api.openai.com/auth", {}).get("chatgpt_plan_type") != "pro":
        raise RuntimeError("Account is outside the verified composer prediction eligibility")
    source_thread_id = str(uuid.UUID(request["source_thread_id"]))
    source_turn_id = str(uuid.UUID(request["source_turn_id"]))
    trigger = request["prediction_trigger"]
    if trigger not in ("live", "catch_up"):
        raise RuntimeError("Invalid prediction trigger; no fallback")

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            raise RuntimeError("Unexpected prediction configuration redirect")

    config_request = urllib.request.Request(
        "https://chatgpt.com/backend-api/wham/predictions/config",
        headers={"Authorization": "Bearer " + tokens["access_token"],
                 "ChatGPT-Account-Id": tokens["account_id"],
                 "User-Agent": "codex_cli_official_predictions/1", "Accept": "application/json"},
    )
    with urllib.request.build_opener(NoRedirect()).open(config_request, timeout=10) as response:
        prediction_config = json.load(response)
    if prediction_config.get("is_enabled") is not True or model in prediction_config.get("unsupported_models", []):
        raise RuntimeError("Official service has not enabled predictions for this account/model")
    # Match the official per-trigger rollout, including nullish fallback semantics.
    rate = prediction_config.get("live_sample_rate" if trigger == "live" else "catch_up_sample_rate")
    if rate is None:
        rate = prediction_config.get("sample_rate")
    if rate is None:
        rate = 1
    if not isinstance(rate, (int, float)) or not 0 <= rate <= 1:
        raise RuntimeError("Invalid official prediction sampling configuration")
    if rate < 1 and __import__("random").random() >= rate:
        print("sampled_out", file=sys.stderr, flush=True)
        print(json.dumps({"suggestion": None}), flush=True)
        return
    prompt = prediction_config["prompt"]
    effort = prediction_config["prediction_reasoning_effort"] or None
    if not isinstance(prompt, str) or not prompt or len(prompt) > 20000:
        raise RuntimeError("Invalid official prediction prompt")
    if effort not in (None, "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"):
        raise RuntimeError("Unsupported official prediction effort")
    items = request["items"]
    if not 1 <= len(items) <= 8 or items[-1].get("role") != "assistant":
        raise RuntimeError("Invalid prediction context")
    for item in items:
        if item.get("type") != "message" or item.get("role") not in ("user", "assistant"):
            raise RuntimeError("Only conversation text can seed predictions")
        expected = "input_text" if item["role"] == "user" else "output_text"
        if len(item.get("content", [])) != 1:
            raise RuntimeError("Invalid message content")
        content = item["content"][0]
        if content.get("type") != expected or not isinstance(content.get("text"), str) or len(content["text"]) > 1500:
            raise RuntimeError("Invalid prediction text")

    with tempfile.TemporaryDirectory(prefix="companion-", dir=request["runtime_root"]) as temporary:
        root = pathlib.Path(temporary)
        isolated_auth = {
            "auth_mode": "chatgpt",
            "tokens": {key: tokens[key] for key in ("access_token", "id_token", "account_id")},
            "last_refresh": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        }
        # Do not rotate the shared login's refresh token from an isolated process.
        isolated_auth["tokens"]["refresh_token"] = ""
        with open(str(root / "auth.json"), "w", opener=lambda path, flags: os.open(path, flags, 0o600)) as stream:
            json.dump(isolated_auth, stream)
        disabled = [
            "apps", "code_mode", "code_mode_only", "context_management", "current_time_reminder",
            "deferred_executor", "enable_fanout", "goals", "hooks", "image_generation", "memories",
            "multi_agent", "multi_agent_v2", "plugins", "request_permissions_tool", "shell_snapshot",
            "shell_tool", "standalone_web_search", "token_budget", "tool_suggest", "unified_exec", "view_image",
        ]
        (root / "config.toml").write_text(
            'model_provider="openai"\nmodel=' + json.dumps(model) + '\n'
            'cli_auth_credentials_store="file"\napproval_policy="never"\nsandbox_mode="read-only"\n'
            'check_for_update_on_startup=false\nweb_search="disabled"\n'
            '[features]\n' + ''.join(key + '=false\n' for key in disabled) +
            '[cloud.skills]\nenabled=false\n[skills]\ninclude_instructions=false\n'
            '[tools.experimental_request_user_input]\nenabled=false\n[tools.update_plan]\nenabled=false\n',
            encoding="utf-8",
        )
        allowed = ("PATH", "LANG", "LC_ALL", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
                   "http_proxy", "https_proxy", "all_proxy", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR",
                   "CODEX_CA_CERTIFICATE")
        env = {key: os.environ[key] for key in allowed if key in os.environ}
        env.update(HOME=temporary, CODEX_HOME=temporary, CODEX_SQLITE_HOME=temporary + "/sqlite")
        owner = os.getpid()

        def stop_with_parent():
            # If the TUI cancels and kills this helper, its backend must not survive.
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.prctl(1, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != owner:
                os._exit(1)

        backend_process = subprocess.Popen(
            [str(backend), "app-server", "--listen", "stdio://"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, cwd=temporary, env=env,
            universal_newlines=True, encoding="utf-8", bufsize=1, start_new_session=True,
            preexec_fn=stop_with_parent,
        )
        events = queue.Queue()
        pending = []

        def collect():
            try:
                for line in backend_process.stdout:
                    events.put(json.loads(line))
            finally:
                events.put({"eof": True})

        threading.Thread(target=collect, daemon=True).start()
        sequence = 0
        deadline = time.monotonic() + 40

        def send(value):
            backend_process.stdin.write(json.dumps(value) + "\n")
            backend_process.stdin.flush()

        def receive():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("Prediction deadline exceeded; no fallback")
            row = events.get(timeout=remaining)
            if row.get("eof"):
                raise RuntimeError("Official prediction backend exited")
            if "id" in row and "method" in row:
                raise RuntimeError("Interactive request forbidden during prediction")
            return row

        def rpc(method, params):
            nonlocal sequence
            sequence += 1
            send({"id": sequence, "method": method, "params": params})
            while True:
                row = receive()
                if row.get("id") == sequence:
                    if "result" not in row:
                        # Errors can contain account/server details; report only the failed method.
                        raise RuntimeError("Official prediction RPC failed: " + method)
                    return row["result"]
                pending.append(row)

        try:
            rpc("initialize", {"clientInfo": {"name": "codex_cli_official_predictions", "version": "1"},
                               "capabilities": {"experimentalApi": True}})
            send({"method": "initialized", "params": {}})
            account = rpc("account/read", {"refreshToken": False}).get("account") or {}
            if account.get("type") != "chatgpt" or account.get("planType") != "pro":
                raise RuntimeError("Eligible ChatGPT login was not confirmed")
            source = rpc("thread/start", {
                "model": model, "modelProvider": "openai", "allowProviderModelFallback": False,
                "cwd": temporary, "approvalPolicy": "never", "sandbox": "read-only", "ephemeral": False,
                "threadSource": "cli_prediction_seed",
                "dynamicTools": [], "selectedCapabilityRoots": [], "environments": [],
            })
            if source.get("modelProvider") != "openai" or source.get("model") != model:
                raise RuntimeError("Prediction provider/model changed unexpectedly")
            rpc("thread/inject_items", {"threadId": source["thread"]["id"], "items": items})
            fork = rpc("thread/fork", {
                "threadId": source["thread"]["id"], "experimentalPredictionMode": True,
                "ephemeral": True, "excludeTurns": True, "threadSource": "composer_predictions",
            })
            if fork.get("modelProvider") != "openai" or fork.get("model") != model:
                raise RuntimeError("Official prediction fork changed provider/model")
            prediction_id = fork["thread"]["id"]
            schema = {"type": "object", "properties": {"suggestion": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
                      "required": ["suggestion"], "additionalProperties": False,
                      "$schema": "http://json-schema.org/draft-07/schema#"}
            # The sole inference turn is on the accepted official prediction fork.
            rpc("turn/start", {
                "threadId": prediction_id, "turnTrigger": "composer_predictions",
                "clientUserMessageId": str(uuid.uuid4()),
                "input": [{"type": "text", "text": prompt + "\n\nReturn only JSON matching this schema: " + json.dumps(schema, separators=(",", ":")) + ".", "text_elements": []}],
                "effort": effort, "outputSchema": None,
                "responsesapiClientMetadata": {"prediction_source_thread_id": source_thread_id,
                    "prediction_source_turn_id": source_turn_id, "prediction_trigger": trigger},
            })
            deadline = min(deadline, time.monotonic() + 30)
            result = None
            while True:
                row = pending.pop(0) if pending else receive()
                params = row.get("params", {})
                if params.get("threadId") != prediction_id:
                    continue
                if row.get("method") == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
                    result = params["item"].get("text")
                if row.get("method") == "turn/completed":
                    if params["turn"].get("status") != "completed" or not result or len(result) > 8000:
                        raise RuntimeError("Official prediction did not complete; no fallback")
                    parsed = json.loads(result)
                    if not isinstance(parsed, dict) or set(parsed) != {"suggestion"}:
                        raise RuntimeError("Invalid prediction response")
                    print(json.dumps(parsed), flush=True)
                    break
        finally:
            try:
                os.killpg(backend_process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                backend_process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(backend_process.pid, signal.SIGKILL)
                backend_process.wait()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Avoid emitting credentials, transcript fragments, or arbitrary server responses.
        reason = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        print("Official prediction unavailable: " + reason + "; no paid fallback", file=sys.stderr)
        sys.exit(1)
