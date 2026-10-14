"""Observe the real controller journey; only the human and independent CI are driven here."""
import json
import os
import re
import subprocess
from pathlib import Path

import production_bootstrap as bootstrap


ARTIFACTS = bootstrap.ARTIFACTS
GITHUB = os.environ['OPENFLOWS_E2E_GITHUB_URL']
GITHUB_CONTAINER = os.environ['OPENFLOWS_E2E_GITHUB_CONTAINER']
CHATS = {}
WORKSPACES = {}


def github(path, body=None, token='ci-operator-token'):
    return bootstrap.request(GITHUB, '/repos/test/repo' + path, body, token)


def docker(*args):
    return subprocess.run(['docker', 'exec', GITHUB_CONTAINER, *args],
        capture_output=True, text=True, check=True, timeout=120).stdout.strip()


def save(filename, data):
    (ARTIFACTS / filename).write_text(json.dumps(data, indent=2))


def observe():
    workspaces = bootstrap.coder('/workspaces')['workspaces']
    for workspace in workspaces:
        WORKSPACES[workspace['id']] = workspace
        for resource in workspace['latest_build']['resources']:
            for agent in resource.get('agents') or []:
                logs = bootstrap.coder('/workspaceagents/' + agent['id'] + '/logs?after=0')
                save('agent-' + agent['id'] + '.json', logs)
                if agent.get('lifecycle_state') in ('start_error', 'start_timeout'):
                    raise RuntimeError(f'Production workspace startup failed: {workspace["name"]}; see agent logs')
    for chat in bootstrap.coder('/chats'):
        CHATS[chat['id']] = chat
        messages = bootstrap.coder('/chats/' + chat['id'] + '/messages?limit=100')
        save('chat-' + chat['id'] + '.json', {'state': chat, 'messages': messages})
        errors = []
        for message in messages['messages']:
            for part in message.get('content', []):
                if part['type'] != 'tool-result':
                    continue
                result = part.get('result') or {}
                if isinstance(result, str):
                    try:
                        result = json.loads(result)
                    except ValueError:
                        result = {}
                failed = isinstance(result, dict) and (result.get('success') is False or
                    result.get('exit_code', 0) not in (0, None))
                if part.get('is_error') or failed:
                    errors.append(part)
        if errors:
            raise RuntimeError(f'Controller-created chat had a failed tool call: {errors[0]}')
        if chat['status'] == 'error':
            raise RuntimeError(f'Controller-created chat failed: {chat}')
    save('workspaces.json', list(WORKSPACES.values()))
    return workspaces


def operator_status():
    result = subprocess.run(['openflows', 'status', '--tenant', bootstrap.TENANT, '--json'],
        capture_output=True, text=True, timeout=30, check=True)
    data = json.loads(result.stdout)
    save('operator-status.json', data)
    return data[0]


def lifecycle(nexus):
    command = 'env OPENFLOWS_TICKET=T-001 OPENFLOWS_ROLE=forge /opt/openflows-dev/openflows-harness status get'
    state = json.loads(bootstrap.ssh(nexus, command))
    save('lifecycle.json', state)
    return state


def ci(sha):
    docker('python3', '/opt/fixtures/run_ci.py', '--api', 'http://localhost:8080',
        '--remote', 'git://localhost:9418/repo.git', '--sha', sha,
        '--oracle', '/opt/fixtures/acceptance.sh', '--artifacts', '/tmp/github/ci-artifacts')


def journal():
    return [json.loads(line) for line in docker('cat', '/tmp/github/github-requests.jsonl').splitlines()]


def assert_no_merge_attempt():
    assert not any(entry['method'] == 'PUT' and entry['path'].endswith('/merge') for entry in journal()), \
        'The controller attempted a merge before all gates passed'


def run():
    workspace, templates, organization, nexus = bootstrap.setup_tenant('system-e2e')
    try:
        original_main = docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', 'refs/heads/main')
        issue = github('/issues', {'title': 'Fix answer.txt to return 42',
            'body': 'The committed answer is 41. Change it to 42, preserve the CI workflow, '
                'and verify the exact committed candidate through SENTINEL before opening a PR.'})
        assert issue['number'] == 1
        save('issue.json', issue)
        print('Created the issue through GitHub; waiting for actual NEXUS assignment and role chats.', flush=True)

        def pull_opened():
            observe()
            pulls = github('/pulls')
            return pulls[0] if pulls else False
        pr = bootstrap.wait('NEXUS/FORGE/SENTINEL did not reach an actual PR', pull_opened, seconds=600)
        assert len(github('/pulls')) == 1
        assert docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', 'refs/heads/main') == original_main, \
            'Agent work changed the default branch before the merge gates passed'
        save('candidate-pr.json', pr)
        head = pr['head']['sha']
        assert docker('git', '--git-dir=/tmp/github/repo.git', 'show', head + ':answer.txt') == '42'

        def reviewed():
            observe()
            state = lifecycle(nexus)
            return state if (state.get('pr_decision') or {}).get('approved') else False
        state = bootstrap.wait('SENTINEL did not approve its PR review', reviewed)
        assert state['head'] == head and state['verified_head'] == head
        assert state['plan_decision']['approved'] and state['test_decision']['approved']
        assert not state.get('pr_human')
        for ws in bootstrap.coder('/workspaces')['workspaces']:
            if ws['id'] == workspace['id']:
                continue
            name = 'ci/' + ws['name']
            if 'forge' in ws['name']:
                (ARTIFACTS / 'forge-verify.log').write_text(bootstrap.ssh(name,
                    'cat /home/coder/.local/state/openflows/verify.log'))
        verification = []
        for chat in CHATS.values():
            messages = bootstrap.coder('/chats/' + chat['id'] + '/messages?limit=100')
            for message in messages['messages']:
                for part in message.get('content', []):
                    if part['type'] != 'tool-result' or part.get('tool_name') != 'execute':
                        continue
                    result = part.get('result') or {}
                    if isinstance(result, str):
                        result = json.loads(result)
                    try:
                        evidence = json.loads(result.get('output', ''))
                    except ValueError:
                        continue
                    if isinstance(evidence, dict) and 'head_sha' in evidence and 'executor' in evidence:
                        verification.append(evidence)
        assert len(verification) == 1
        evidence = verification[0]
        forge = next(ws for ws in WORKSPACES.values() if 'forge' in ws['name'])
        assert evidence['executor']['workspace'] == forge['id']
        assert evidence['head_sha'] == head and evidence['exit_code'] == 0
        assert 'ACCEPTANCE_OK' in evidence['stdout']
        cwd = next(line.split('=', 1)[1] for line in evidence['stdout'].splitlines() if line.startswith('CHECKOUT_CWD='))
        assert not cwd.startswith('/home/coder/workspace')
        assert evidence['task_id'] == state['verification_task']
        save('verification.json', evidence)
        print('Actual role chats planned, implemented, verified over A2A, and opened/reviewed the PR.', flush=True)

        ci(head)
        hold_start = len(bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log').splitlines())
        def held_for_human():
            assert not github('/pulls/' + str(pr['number']))['merged'], 'Merged without human approval'
            observe()
            assert_no_merge_attempt()
            log = re.sub(r'\x1b\[[0-9;]*m', '', bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log'))
            return any('Deferring PR: waiting for lifecycle approvals or review delivery' in line and
                'human_approved=false' in line and 'review_approved=true' in line
                for line in log.splitlines()[hold_start:])
        bootstrap.wait('VESSEL did not hold the reviewed PR for human approval', held_for_human, seconds=90)
        save('before-human.json', lifecycle(nexus))
        assert not lifecycle(nexus).get('pr_human')
        assert_no_merge_attempt()
        # A pending check is a declared external state, never a fabricated success.
        github('/check-runs', {'name': 'acceptance', 'head_sha': head, 'status': 'in_progress'}, 'ci-runner-token')
        subprocess.run(['openflows', 'gate', 'decide', '--tenant', bootstrap.TENANT, '--ticket', 'T-001',
            '--phase', 'submit', '--revision', str(state['revision']), '--round', str(state['review_round']),
            '--head', head, '--verdict', 'approve', '--notes', 'Operator approves the verified candidate'],
            check=True, timeout=30)
        github('/pulls/' + str(pr['number']) + '/reviews',
            {'event': 'APPROVE', 'commit_id': head, 'body': 'Human approval of the verified candidate'})
        bootstrap.wait('Approved SENTINEL review delivery did not complete',
            lambda: not lifecycle(nexus).get('pr_delivery'), seconds=90)
        pending_start = len(journal())
        pending_log_start = len(bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log').splitlines())
        def polled_pending_ci():
            assert not github('/pulls/' + str(pr['number']))['merged'], 'Merged while CI was pending'
            assert_no_merge_attempt()
            polled = any(entry['method'] == 'GET' and entry['path'] == f'/repos/test/repo/commits/{head}/check-suites' and
                any(check['status'] == 'in_progress' for check in entry['response'].get('check_suites', []))
                for entry in journal()[pending_start:])
            log = re.sub(r'\x1b\[[0-9;]*m', '', bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log'))
            waiting = any('Waiting for CI before merge' in line and f'head={head}' in line and
                f"pr={pr['number']}" in line for line in log.splitlines()[pending_log_start:])
            assert_no_merge_attempt()
            return polled and waiting
        bootstrap.wait('VESSEL did not wait for pending CI after review delivery', polled_pending_ci, seconds=90)
        print('Human approval and pending CI each prevented premature merging; running trusted acceptance.', flush=True)
        ci(head)
        merged_pr = bootstrap.wait('Actual VESSEL did not merge the approved and tested candidate',
            lambda: (current if (current := github('/pulls/' + str(pr['number'])))['merged'] else False))
        merged = merged_pr['merge_commit_sha']
        assert docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', 'main') == merged
        assert docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', merged + '^') == pr['base']['sha']
        assert docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', merged + '^{tree}') == \
            docker('git', '--git-dir=/tmp/github/repo.git', 'rev-parse', head + '^{tree}')
        ci(merged)

        def cleaned():
            remaining = observe()
            status = operator_status()
            ticket = next(t for t in status['tickets'] if t['id'] == 'T-001')
            return len(remaining) == 1 and remaining[0]['id'] == workspace['id'] and \
                ticket['status']['type'] == 'merged'
        bootstrap.wait('Controller did not complete the ticket and remove both role workspaces', cleaned)
        log = bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log')
        (ARTIFACTS / 'controller.log').write_text(log)
        log = re.sub(r'\x1b\[[0-9;]*m', '', log)
        for chat_id in CHATS:
            chat_log = '\n'.join(line for line in log.splitlines() if 'chat_id=' + chat_id in line)
            for event in ('SessionStart', 'PreToolUse', 'PostToolUse', 'Stop'):
                assert 'event=' + event in chat_log, f'No authenticated {event} hook for chat {chat_id}'
        save('system-evidence.json', {'full_issue_to_merge': True, 'issue': issue, 'pr': merged_pr,
            'tested_head': head, 'merged_head': merged, 'role_chats': list(CHATS.values()),
            'workspaces': list(WORKSPACES.values()), 'verification': state['verification_task']})
        bootstrap.coder('/workspaces/' + workspace['id'] + '/builds', {'transition': 'delete'})
        bootstrap.wait('NEXUS deletion did not complete', lambda: not bootstrap.coder('/workspaces')['workspaces'])
        print('PASS: actual issue routing, role chats, signed hooks, A2A verification, human/CI gates, squash merge, acceptance and cleanup.', flush=True)
    finally:
        errors = []
        for collect in (observe, lambda: (ARTIFACTS / 'controller-final.log').write_text(
                bootstrap.ssh(nexus, 'cat /tmp/openflows-controller.log'))):
            try:
                collect()
            except Exception as error:
                errors.append(str(error))
        if errors:
            save('final-diagnostics-errors.json', errors)


if __name__ == '__main__':
    run()
