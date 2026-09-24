import assert from 'node:assert/strict';
import { test } from 'node:test';
import { isOwnerCommand } from './pullfrog-command-check.mjs';

const ownerComment = {
  action: 'created',
  repository: { full_name: 'jatmn/jModGuard' },
  sender: { id: 12479882 },
  comment: { user: { id: 12479882 }, body: '@pullfrog review this PR' },
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
