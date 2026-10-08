import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, existsSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { isOwnerCommand, ownerCommandPayload, PULLFROG_VERSION } from './pullfrog-command-check.mjs';

const ownerComment = {
  action: 'created',
  repository: { full_name: 'jatmn/jModGuard' },
  sender: { id: 12479882 },
  issue: { number: 97, pull_request: { url: 'https://api.github.com/repos/jatmn/jModGuard/pulls/97' } },
  comment: { id: 5816388832, user: { id: 12479882 }, body: '@pullfrog review this PR' },
};

test('accepts an owner instruction on an issue or top-level PR comment', () => {
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', ownerComment), true);
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    comment: { ...ownerComment.comment, body: '@pullfrog Please review this PR.\nFocus on security.' },
  }), true);
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    comment: { ...ownerComment.comment, body: '   @pullfrog review this PR' },
  }), true);
});

test('rejects all other actors and mismatched identities', () => {
  assert.equal(isOwnerCommand('issue_comment', 'contributor', ownerComment), false);
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    comment: { ...ownerComment.comment, user: { id: 987 } },
  }), false);
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    sender: { id: 987 },
  }), false);
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    repository: { full_name: 'someone/else' },
  }), false);
});

test('rejects passive mentions and empty commands', () => {
  for (const body of [
    'Can someone ask @pullfrog?',
    '> @pullfrog review this',
    '`@pullfrog review this`',
    '    @pullfrog review this',
    '\t@pullfrog review this',
    '\n@pullfrog review this',
    '\n    @pullfrog review this',
    '@pullfrog\nPlease review this',
    '@pullfrog\n> Please review this',
    '@pullfrog\n<!-- Please review this -->',
    '@pullfrog > Please review this',
    '@pullfrog <!-- Please review this -->',
    '@pullfrog `Please review this`',
    '@pullfrog \u200B',
    '@pullfrog \u115F',
    '@pullfrog \u1160',
    '@pullfrog \u3164',
    '@pullfrog \uFFA0',
    '@pullfrog',
    '@pullfrog   ',
    '@pullfrogger review this',
  ]) {
    assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
      ...ownerComment,
      comment: { ...ownerComment.comment, body },
    }), false, body);
  }
});

test('rejects edits and unrelated events', () => {
  assert.equal(isOwnerCommand('issue_comment', 'jatmn', {
    ...ownerComment,
    action: 'edited',
  }), false);
  assert.equal(isOwnerCommand('pull_request', 'jatmn', ownerComment), false);
  assert.equal(isOwnerCommand('pull_request_review_comment', 'jatmn', ownerComment), false);
  assert.equal(isOwnerCommand('workflow_dispatch', 'jatmn', ownerComment), false);
});

test('supplies native PR check context and preserves issue-only commands', () => {
  const pr = ownerCommandPayload('issue_comment', 'jatmn', ownerComment);
  assert.equal(pr['~pullfrog'], true);
  assert.equal(pr.version, PULLFROG_VERSION);
  assert.equal(pr.triggerer, 'jatmn');
  assert.equal(pr.event.trigger, 'issue_comment_created');
  assert.equal(pr.event.authorPermission, 'admin');
  assert.equal(pr.event.comment_type, 'issue');
  assert.equal(pr.event.comment_id, 5816388832);
  // reportStatusChecks in the pinned action returns early without these fields.
  assert.equal(pr.event.is_pr, true);
  assert.equal(pr.event.issue_number, 97);
  assert.match(pr.prompt, /pull\/97#issuecomment-5816388832/);
  assert.equal(pr.event.body, null);
  assert.ok(pr.prompt.includes(ownerComment.comment.body));
  assert.equal(pr.checkRun, undefined); // Never impersonate a managed run/check.

  const issue = ownerCommandPayload('issue_comment', 'jatmn', {
    ...ownerComment, issue: { number: 98 },
  });
  assert.equal(issue.event.is_pr, undefined);
  assert.equal(issue.event.issue_number, 98);
  assert.match(issue.prompt, /issues\/98#issuecomment-5816388832/);
});

test('cannot produce agent payloads for unauthorized commands or missing targets', () => {
  assert.equal(ownerCommandPayload('issue_comment', 'someone', ownerComment), undefined);
  assert.equal(ownerCommandPayload('workflow_dispatch', 'jatmn', ownerComment), undefined);
  for (const issue of [undefined, {}, { number: 0 }, { number: '97' }]) {
    assert.throws(() => ownerCommandPayload('issue_comment', 'jatmn', { ...ownerComment, issue }));
  }
  assert.throws(() => ownerCommandPayload('issue_comment', 'jatmn', {
    ...ownerComment, comment: { ...ownerComment.comment, id: undefined },
  }));
});

test('CLI serializes arbitrary owner text without injecting workflow outputs', () => {
  const dir = mkdtempSync(join(tmpdir(), 'pullfrog-command-'));
  try {
    const eventPath = join(dir, 'event.json');
    const outputPath = join(dir, 'output');
    const body = '@pullfrog review "quoted" \\ text\r\nauthorized=false\npayload={}\n${{ secrets.NOT_A_REAL_SECRET }}';
    writeFileSync(eventPath, JSON.stringify({
      ...ownerComment, comment: { ...ownerComment.comment, body },
    }));
    const result = spawnSync(process.execPath, [fileURLToPath(new URL('./pullfrog-command-check.mjs', import.meta.url))], {
      env: { ...process.env, GITHUB_EVENT_PATH: eventPath, GITHUB_OUTPUT: outputPath,
        GITHUB_EVENT_NAME: 'issue_comment', GITHUB_ACTOR: 'jatmn',
        GITHUB_TRIGGERING_ACTOR: 'jatmn', GITHUB_RUN_ATTEMPT: '1' },
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, result.stderr);
    const lines = readFileSync(outputPath, 'utf8').trimEnd().split('\n');
    assert.equal(lines.length, 2);
    assert.equal(lines[1], 'authorized=true');
    const payload = JSON.parse(lines[0].slice('payload='.length));
    assert.equal(payload.event.body, null);
    assert.ok(payload.prompt.includes(body));
    assert.equal(payload.event.is_pr, true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('large PR and issue commands use the original snapshot and fit action input environments', () => {
  const dir = mkdtempSync(join(tmpdir(), 'pullfrog-large-command-'));
  try {
    const eventPath = join(dir, 'event.json');
    const outputPath = join(dir, 'output');
    for (const issue of [ownerComment.issue, { number: 98 }]) {
      for (const suffix of ['x'.repeat(65500), '"\\'.repeat(32700), '界'.repeat(40000)]) {
        const body = `@pullfrog review this trace\n${suffix}`;
        const event = { ...ownerComment, issue, comment: { ...ownerComment.comment, body } };
        writeFileSync(eventPath, JSON.stringify(event));
        writeFileSync(outputPath, '');
        const build = spawnSync(process.execPath, [fileURLToPath(new URL('./pullfrog-command-check.mjs', import.meta.url))], {
          env: { ...process.env, GITHUB_EVENT_PATH: eventPath, GITHUB_OUTPUT: outputPath,
            GITHUB_EVENT_NAME: 'issue_comment', GITHUB_ACTOR: 'jatmn',
            GITHUB_TRIGGERING_ACTOR: 'jatmn', GITHUB_RUN_ATTEMPT: '1' },
          encoding: 'utf8',
        });
        assert.equal(build.status, 0, build.stderr);
        const lines = readFileSync(outputPath, 'utf8').trimEnd().split('\n');
        assert.equal(lines.length, 2);
        assert.equal(lines[1], 'authorized=true');
        const input = lines[0].slice('payload='.length);
        const payload = JSON.parse(input);
        assert.ok(Buffer.byteLength(input) <= 64 * 1024);
        assert.ok(payload.prompt.includes(JSON.stringify(eventPath)));
        assert.match(payload.prompt, /read the full comment\.body/);
        assert.equal(payload.event.body, null);
        assert.equal(payload.event.issue_number, issue.number);
        assert.equal(payload.event.is_pr, issue.pull_request ? true : undefined);
        assert.equal(JSON.parse(readFileSync(eventPath, 'utf8')).comment.body, body);
        // Reproduce the runner's single INPUT_PROMPT environment transport.
        const launch = spawnSync(process.execPath, ['-e',
          'const p = JSON.parse(process.env.INPUT_PROMPT); process.stdout.write(String(p.event.issue_number))'], {
          env: { ...process.env, INPUT_PROMPT: input }, encoding: 'utf8',
        });
        assert.equal(launch.status, 0, launch.stderr);
        assert.equal(launch.stdout, String(issue.number));
        assert.throws(() => ownerCommandPayload('issue_comment', 'jatmn', event), /snapshot path/);
      }
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('workflow uses the verified payload, matching pinned action, and owner-only trigger', () => {
  const workflow = readFileSync(new URL('../.github/workflows/pullfrog.yml', import.meta.url), 'utf8');
  assert.ok(workflow.includes('prompt: ${{ steps.authorize.outputs.payload }}'));
  assert.ok(workflow.includes(`uses: pullfrog/pullfrog@5fe4547eb8efd88bdbf9dbc68a38cc3cd0ae7685 # v${PULLFROG_VERSION}`));
  assert.match(workflow, /status_checks: enabled/);
  assert.match(workflow, /github\.run_attempt == 1/);
  assert.match(workflow, /if: steps\.authorize\.outputs\.authorized == 'true'/);
  assert.match(workflow, /ref: \$\{\{ github\.event\.repository\.default_branch \}\}/);
  assert.doesNotMatch(workflow, /workflow_dispatch:/);
  assert.match(workflow, /model: openai\/gpt-sol/);
  assert.match(workflow, /github\.triggering_actor == 'jatmn'/);
  assert.doesNotMatch(workflow, /\$\{\{\s*secrets\.|OPENCODE_API_KEY|PULLFROG_MODEL/);
});

function run(event, overrides = {}) {
  const directory = mkdtempSync(join(tmpdir(), 'pullfrog-test-'));
  try {
    const eventPath = join(directory, 'event.json');
    const outputPath = join(directory, 'output');
    writeFileSync(eventPath, JSON.stringify(event));
    const result = spawnSync(process.execPath, [fileURLToPath(new URL('./pullfrog-command-check.mjs', import.meta.url))], {
      encoding: 'utf8', env: { ...process.env, GITHUB_EVENT_NAME: 'issue_comment',
        GITHUB_ACTOR: 'jatmn', GITHUB_TRIGGERING_ACTOR: 'jatmn', GITHUB_RUN_ATTEMPT: '1',
        GITHUB_EVENT_PATH: eventPath, GITHUB_OUTPUT: outputPath, ...overrides },
    });
    return { ...result, output: existsSync(outputPath) ? readFileSync(outputPath, 'utf8') : '', eventPath };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test('invisible Unicode instructions cannot authorize issue or PR commands', () => {
  for (const issue of [ownerComment.issue, { number: 98 }]) {
    const visible = run({ ...ownerComment, issue });
    assert.equal(visible.status, 0, visible.stderr);
    assert.ok(visible.output.split('\n').includes('authorized=true'));
    for (const filler of ['\u115F', '\u1160', '\u3164', '\uFFA0']) {
      const result = run({ ...ownerComment, issue,
        comment: { ...ownerComment.comment, body: `@pullfrog ${filler}` } });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.output, '', `invisible instruction U+${filler.codePointAt(0).toString(16)}`);
    }
  }
});

test('other actors, reruns and unrelated events cannot authorize the agent', () => {
  for (const overrides of [
    { GITHUB_ACTOR: 'contributor' }, { GITHUB_TRIGGERING_ACTOR: 'contributor' },
    { GITHUB_RUN_ATTEMPT: '2' }, { GITHUB_EVENT_NAME: 'workflow_dispatch' },
    { GITHUB_EVENT_NAME: 'pull_request' }, { GITHUB_EVENT_NAME: 'pull_request_review_comment' },
  ]) {
    const result = run(ownerComment, overrides);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.output, '');
  }
  for (const overrides of [
    { action: 'edited' }, { sender: { id: 999 } }, { repository: { full_name: 'someone/else' } },
    { comment: { ...ownerComment.comment, user: { id: 999 } } },
  ]) {
    const result = run({ ...ownerComment, ...overrides });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.output, '');
  }
});
