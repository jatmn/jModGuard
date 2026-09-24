import { appendFileSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const OWNER_ID = 12479882;
const REPOSITORY = 'jatmn/jModGuard';
// Matches the pinned action in .github/workflows/pullfrog.yml. Pullfrog's
// structured prompt is version-specific; verify its schema when updating both.
export const PULLFROG_VERSION = '0.1.82';
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

export function ownerCommandPayload(eventName, actor, event) {
  if (!isOwnerCommand(eventName, actor, event)) return undefined;
  const issueNumber = event.issue?.number;
  const commentId = event.comment.id;
  if (!Number.isSafeInteger(issueNumber) || issueNumber <= 0 ||
      !Number.isSafeInteger(commentId) || commentId <= 0) {
    throw new Error('Owner command is missing a valid issue number or comment ID');
  }
  const isPr = Boolean(event.issue.pull_request);
  const url = `https://github.com/${REPOSITORY}/${isPr ? 'pull' : 'issues'}/${issueNumber}#issuecomment-${commentId}`;
  return {
    '~pullfrog': true,
    version: PULLFROG_VERSION,
    triggerer: actor,
    prompt: `The repository owner issued this command in ${REPOSITORY}:\n${event.comment.body}\n\n` +
      `Read the linked issue or pull request and current repository state before acting. ` +
      `The command is at ${url}. Post your result in that thread.`,
    event: {
      trigger: 'issue_comment_created',
      comment_type: 'issue',
      comment_id: commentId,
      issue_number: issueNumber,
      ...(isPr ? { is_pr: true } : {}),
      body: event.comment.body,
    },
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  const payload = ownerCommandPayload(process.env.GITHUB_EVENT_NAME, process.env.GITHUB_ACTOR, event);
  if (payload) {
    // JSON.stringify escapes newlines in owner text so it cannot add output keys.
    appendFileSync(process.env.GITHUB_OUTPUT, `payload=${JSON.stringify(payload)}\nauthorized=true\n`);
  }
}
