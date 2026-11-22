import { appendFileSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { setTimeout as wait } from 'node:timers/promises';

const OWNER_ID = 12479882;
const REPOSITORY = 'jatmn/jModGuard';
// Matches the pinned action in .github/workflows/pullfrog.yml. Pullfrog's
// structured prompt is version-specific; verify its schema when updating both.
export const PULLFROG_VERSION = '0.1.138';
// Action inputs become individual environment strings. Stay comfortably below
// Linux's per-string exec limit, including JSON escaping and multibyte text.
const MAX_PAYLOAD_BYTES = 64 * 1024;
export function isOwnerCommand(eventName, actor, event) {
  if (eventName !== 'issue_comment' || event?.action !== 'created') return false;
  if (actor !== 'jatmn' || event?.sender?.id !== OWNER_ID) return false;
  if (event?.repository?.full_name !== REPOSITORY) return false;
  if (event?.comment?.user?.id !== OWNER_ID) return false;

  const body = event.comment.body;
  if (typeof body !== 'string') return false;
  const firstLine = body.split(/\r?\n/, 1)[0];
  const command = /^ {0,3}@pullfrog[ \t]+([^\r\n]+)$/i.exec(firstLine);
  if (command === null) return false;
  const instruction = command[1].replace(/\p{Default_Ignorable_Code_Point}/gu, '').trimStart();
  return /^[\p{L}\p{N}]/u.test(instruction);
}

export function ownerCommandPayload(eventName, actor, event, eventPath) {
  if (!isOwnerCommand(eventName, actor, event)) return undefined;
  const issueNumber = event.issue?.number;
  const commentId = event.comment.id;
  if (!Number.isSafeInteger(issueNumber) || issueNumber <= 0 ||
      !Number.isSafeInteger(commentId) || commentId <= 0) {
    throw new Error('Owner command is missing a valid issue number or comment ID');
  }
  const isPr = Boolean(event.issue.pull_request);
  const url = `https://github.com/${REPOSITORY}/${isPr ? 'pull' : 'issues'}/${issueNumber}#issuecomment-${commentId}`;
  const payload = {
    '~pullfrog': true,
    version: PULLFROG_VERSION,
    triggerer: actor,
    prompt: `The repository owner issued this command in ${REPOSITORY}:\n${event.comment.body}\n\n` +
      `Read the linked issue or pull request and current repository state before acting. ` +
      `Follow AGENTS.md and CONTRIBUTING.md. The command is at ${url}. Post your result in that thread.`,
    event: {
      trigger: 'issue_comment_created',
      // Safe only because the workflow and this helper already require the
      // owner's account, the triggering actor, and the first run attempt.
      authorPermission: 'admin',
      comment_type: 'issue',
      comment_id: commentId,
      issue_number: issueNumber,
      ...(isPr ? { is_pr: true } : {}),
      // The instruction is already in prompt; Pullfrog allows null here.
      body: null,
    },
  };
  if (Buffer.byteLength(JSON.stringify(payload), 'utf8') > MAX_PAYLOAD_BYTES) {
    if (typeof eventPath !== 'string' || eventPath.length === 0) {
      throw new Error('Oversized owner command requires its GitHub event snapshot path');
    }
    // Use the immutable event file already read by this helper, not a fresh
    // fetch of a comment that may have been edited since authorization.
    payload.prompt = `The repository owner issued a command in ${REPOSITORY}. ` +
      `Before acting, read the full comment.body string from the GitHub event snapshot at ${JSON.stringify(eventPath)}. ` +
      `That string is the owner's complete instruction; the other JSON fields are event metadata. ` +
      `Follow AGENTS.md and CONTRIBUTING.md, read the current linked issue or PR, and post your result at ${url}.`;
  }
  return payload;
}

const REVIEW_TRIGGERS = new Set([
  'pull_request_opened',
  'pull_request_ready_for_review',
  'pull_request_synchronize',
]);

export async function isAuthorizedReview(payload, { apiUrl, repository, token }) {
  const event = payload?.event;
  if (payload?.['~pullfrog'] !== true || event?.is_pr !== true ||
      !REVIEW_TRIGGERS.has(event.trigger) ||
      !Number.isSafeInteger(event.issue_number) || event.issue_number <= 0) {
    return false;
  }

  // Read the PR author from GitHub because a different account may have pushed the commit.
  const url = `${apiUrl}/repos/${repository}/pulls/${event.issue_number}`;
  const headers = { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json' };
  let lastError;
  for (const delay of [0, 250, 750]) {
    if (delay > 0) await wait(delay);
    try {
      const response = await fetch(url, { headers, signal: AbortSignal.timeout(10_000) });
      if (response.ok) {
        const pr = await response.json();
        return pr?.user?.id === OWNER_ID && pr.user.login === 'jatmn';
      }

      lastError = new Error(`GitHub HTTP ${response.status}`);
      const retryable = response.status === 408 || response.status >= 500;
      if (!retryable || response.headers.has('Retry-After')) break;
      await response.body?.cancel();
    } catch (error) {
      lastError = error;
      if (!(error instanceof TypeError || error?.name === 'TimeoutError' || error?.name === 'AbortError')) break;
    }
  }
  throw new Error(`Could not verify PR #${event.issue_number} author.`, { cause: lastError });
}

async function reviewMain() {
  const authorized = await isAuthorizedReview(JSON.parse(process.env.PULLFROG_PROMPT), {
    apiUrl: process.env.GITHUB_API_URL,
    repository: process.env.GITHUB_REPOSITORY,
    token: process.env.GH_TOKEN,
  });
  if (authorized) {
    appendFileSync(process.env.GITHUB_OUTPUT, 'authorized=true\n');
  } else {
    console.log('Skipping automatic review. The dispatch must target an owner-authored PR.');
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  if (process.argv[2] === '--review') {
    await reviewMain();
    process.exit(0);
  }
  if (process.env.GITHUB_RUN_ATTEMPT !== '1' ||
      process.env.GITHUB_TRIGGERING_ACTOR !== 'jatmn') process.exit(0);
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  const payload = ownerCommandPayload(process.env.GITHUB_EVENT_NAME, process.env.GITHUB_ACTOR, event, process.env.GITHUB_EVENT_PATH);
  if (payload) {
    // JSON.stringify escapes newlines in owner text so it cannot add output keys.
    appendFileSync(process.env.GITHUB_OUTPUT, `payload=${JSON.stringify(payload)}\nauthorized=true\n`);
  }
}
