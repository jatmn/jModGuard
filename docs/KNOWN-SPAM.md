# Known spam

Use the dedicated **Known spam** page to save full confirmed spam messages or
URL wildcards. This is separate from Badwords and the Rules list. The page's
rule settings reuse the existing enable, dry-run, skip users/roles/channels,
include channels and skip-moderators controls. Server entries and global entries
combine, just like Badwords. Only the bot owner can add or remove global entries;
server managers can inspect inherited global entries.

Paste a full message, optionally add a note, select the scope, then **Add sample**.
No regex is needed. Entries are not seeded automatically. The page includes both
owner examples below as copyable samples. Saving is a deliberate operator action.

## Message matching

A sample matches the **whole message**, after trimming ends, either raw or through
the existing Badwords `normalize_lenient` and `normalize_compact` helpers. These
fold case, decompose accents, remove supported zero-width characters, map the
existing leetspeak substitutions, and strip all whitespace in the compact form.
They do not implement general confusable recognition or similarity. A lightly
rewritten pitch may not match. Extra sentences, surrounding quotations, different
punctuation and appended URLs can prevent sample equality; add a URL pattern or
another full sample for those variants. Containment matching is intentionally
avoided so quoting a small portion of a pitch does not trigger a ban.

## URL matching

- `tinyurl.com/*` matches any HTTP/HTTPS link on exactly that host, including a
  bare `tinyurl.com` and scheme-less links such as `tinyurl.com/abc`.
- An optional `http://` or `https://` prefix in a pattern is ignored. DNS host
  matching ignores case. There is no implicit `www` or subdomain expansion.
- `*.example.com/*` matches proper subdomains, including nested ones, and excludes
  the apex `example.com`. Only a leading `*.` host wildcard is accepted.
- `example.com/Promo/*` matches that case-sensitive path prefix. Paths use literal
  characters plus `*` for zero or more characters, including `/`. The pattern is
  anchored to the whole parsed path. An omitted path means `/*`; an explicit `/`
  matches only the root path. Percent escapes retain URL-parser spelling; there
  is no percent-decoding.
- Before parsing message links, supported zero-width characters (the same set
  removed by Badwords sample normalization) are removed from hosts and paths in
  both HTTP/HTTPS and scheme-less tokens. No other Unicode/leet normalization is
  performed before parsing. The URL parser's canonicalization still applies,
  including IDNA conversion of Unicode hostnames to ASCII/punycode.
  Scheme recognition, credentials and ports are preserved; cleanup does not
  repair malformed schemes or ports into matching URLs.
- Query strings and fragments in messages are ignored. Patterns cannot contain
  queries, fragments, credentials, ports, whitespace, dot path segments or regexes.
  Pattern hosts must be ASCII DNS names with at least two labels. Message link
  ports do not change the host/path match. Credential-bearing links are excluded.
- Links are parsed from whitespace/Markdown-delimited tokens. Paired outer
  emphasis markers and trailing sentence punctuation (`.,;!?:`) are removed.
  A literal trailing path `*` is retained when there is no paired opening marker.
  Matching runs locally without network lookups or redirects.

A URL hit anywhere in a message has the same action as a saved full-message hit.
Nonmatching hosts and paths do not trigger the list.

## Enforcement and limits

The first hit selects **Ban and purge**, Discord **Previous Hour**, through the
existing ban helper with `delete_message_seconds=3600`. There is no strike ladder
or channel-history scan. This fixed action takes precedence over other detectors'
actions for that message. Actions on the internal rule row do not override it.
Dry-run logs only and never bans or purges. Existing execution-time membership and
moderator protections apply; configuration failure or removal of the matching
entry before dispatch withholds the known-spam action. Configuration propagation
retains the existing polling delay. Permission failures remain ordinary moderation
failures, not evidence of a completed ban.

Each scope allows 1024 entries. Samples are at most 4096 UTF-8 bytes, URL patterns
512 bytes, and optional notes 512 bytes without control characters. Empty samples
(including normalization-empty text) are rejected. Notes are trimmed; blank notes
become absent. Samples with the same compact normalized key in one scope are
duplicates; URL patterns are canonicalized before duplicate checks. A duplicate
is rejected without replacing its note. Server/global duplicates may coexist.
The collection is managed separately and is not included in configuration-copy
operations; rule settings retain normal rule-copy behavior.

Migration **0024** adds an empty core `known_spam` table and a scoped unique index;
it does not alter Badwords or repeated-text data/semantics. The core schema becomes
24; guild schema stays 17. Deploy matching bot/API builds through the existing
paired update protocol. Publication labels derive the new exact core range from
selected source. Older binaries refuse schema24; rollback requires the matching
pre-upgrade backup. No new launcher capability is required.

## Owner examples (examples only)

> 📈 **Looking for a community focused on stocks and trading?**
>
> Join our growing community to learn from experienced investors, share market insights, discuss stock ideas, and exchange trading strategies.
> No spam or aggressive promotion—just practical market discussions.
> We welcome active investors and value your thoughts and feedback.
> 👇 Interested? Join us here:

> I used to work as Full Stack AI developer at several software companies but I’m currently available and working remotely because my last contract ended. Right now I am resting at home. If you have any AI projects or need support, I’d be happy to help, just reach out.
