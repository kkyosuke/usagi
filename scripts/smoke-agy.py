#!/usr/bin/env python3
"""Opt-in AGY 1.2.0 smoke: native system context, MCP, and exact headless resume.

Calls a real model twice. The isolated --gemini_dir switch is intentionally used
only by this developer probe; shipping launches use the normal provider state.
"""

import argparse
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import uuid

if len(sys.argv) > 1 and sys.argv[1] == '--server':
    calls = Path(sys.argv[2])
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if 'id' not in message:
            continue
        method = message.get('method')
        if method == 'initialize':
            result = {'protocolVersion': message['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'agy-smoke', 'version': '1.0.0'}}
        elif method == 'tools/list':
            result = {'tools': [{'name': 'agy_smoke_probe', 'description': 'Read-only integration probe: echo the probe value.', 'inputSchema': {'type': 'object', 'properties': {'probe': {'type': 'string'}}, 'required': ['probe'], 'additionalProperties': False}}]}
        elif method == 'tools/call':
            arguments = message['params']['arguments']
            with calls.open('a') as output:
                output.write(json.dumps(arguments) + '\n')
            result = {'content': [{'type': 'text', 'text': json.dumps(arguments)}]}
        elif method == 'ping':
            result = {}
        else:
            print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32601, 'message': 'Method not found'}}), flush=True)
            continue
        print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), flush=True)
    raise SystemExit(0)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--usagi', required=True, type=Path, help='compiled usagi binary')
parser.add_argument('--work-dir', required=True, type=Path, help='new directory for private smoke artifacts')
parser.add_argument('--agy', default='agy', help='AGY CLI executable')
parser.add_argument('--model', default='gemini-3.8-flash-low')
args = parser.parse_args()
os.umask(0o077)
run = args.work_dir.resolve()
run.mkdir(mode=0o700)  # Refuse reuse so earlier calls cannot satisfy this run.
workspace = run / 'workspace'
workspace.mkdir()
configuration_root = run / 'integration'
configuration = configuration_root / '.agents'
configuration.mkdir(parents=True)
usagi = args.usagi.resolve(strict=True)
(configuration / 'mcp_config.json').write_text(json.dumps({'mcpServers': {'usagi-smoke': {'command': sys.executable, 'args': [str(Path(__file__).resolve()), '--server', str(run / 'calls.jsonl')]}}}))
(configuration / 'hooks.json').write_text(json.dumps({'usagi-smoke': {'PreInvocation': [{'type': 'command', 'command': shlex.quote(str(usagi)) + ' agy-system-prompt', 'timeout': 3}]}}))
env = os.environ.copy()
env['AGY_CLI_DISABLE_AUTO_UPDATE'] = '1'
(run / 'gemini-state').mkdir()
(run / 'tmp').mkdir()
env['TMPDIR'] = str(run / 'tmp')
memory = 'MEMORY_' + uuid.uuid4().hex
base = [args.agy, '--gemini_dir', str(run / 'gemini-state'), '--add-dir', str(configuration_root), '--log-file', '/dev/null', '--mode', 'accept-edits', '--dangerously-skip-permissions', '--model', args.model, '--output-format', 'json', '--print-timeout', '60s']

def invoke(number, task, conversation=None):
    token = f'SYSTEM_CONTEXT_{number}'
    env['USAGI_AGY_SYSTEM_PROMPT'] = f'You are a test role. Call the agy_smoke_probe MCP tool exactly once using the memory token the user supplied earlier or in this turn as its probe value. After the tool succeeds, reply with exactly {token}. Do not read or change files.'
    command = base + (['--conversation', conversation] if conversation else []) + ['--print', task]
    child = subprocess.Popen(command, cwd=workspace, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = child.communicate(timeout=75)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            stdout, stderr = child.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            stdout, stderr = child.communicate()
        (run / f'{number}.stderr').write_bytes(stderr)
        (run / f'{number}.stdout').write_bytes(stdout)
        raise RuntimeError('Native AGY timed out; inspect private worktree-local logs')
    (run / f'{number}.stderr').write_bytes(stderr)
    (run / f'{number}.stdout').write_bytes(stdout)
    result = json.loads(stdout)
    if child.returncode != 0 or result.get('status') != 'SUCCESS' or result.get('response', '').strip() != token:
        raise RuntimeError(f'Native turn {number} failed validation; inspect private worktree-local logs')
    return result['conversation_id']

conversation = invoke(1, f'Remember this token for later: {memory}. Follow the current test role to complete its MCP probe.')
resumed = invoke(2, 'Use the memory token from the previous turn as the MCP probe. Follow the current test role to complete its MCP probe.', conversation)
calls = [json.loads(line) for line in (run / 'calls.jsonl').read_text().splitlines()]
assert resumed == conversation, 'AGY changed the conversation identity during resume'
assert calls == [{'probe': memory}, {'probe': memory}], 'MCP did not receive the remembered token on both turns'
print(json.dumps({'native_model_response': True, 'transient_system_role': True, 'mcp_probe': True, 'headless_exact_resume': True, 'role_refreshed_on_resume': True, 'conversation_memory': True, 'model': args.model}))
