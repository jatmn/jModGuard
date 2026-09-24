import { appendFileSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const OWNER_ID = 12479882;
const REPOSITORY = 'jatmn/jModGuard';
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

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  if (isOwnerCommand(process.env.GITHUB_EVENT_NAME, process.env.GITHUB_ACTOR, event)) {
    appendFileSync(process.env.GITHUB_OUTPUT, 'authorized=true\n');
  }
}
