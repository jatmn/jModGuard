import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { relative } from "node:path";
import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const allowlistPath = "tools/test-flake-allowlist.tsv";
const git = (...args) => execFileSync("git", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
const rev = (ref) => {
    try {
        return git("rev-parse", "--verify", "--quiet", ref + "^{commit}").trim();
    } catch (error) {
        if (error.status !== 1 && error.status !== 128) throw error;
        return undefined;
    }
};

function baseRevision() {
    const eventBase = process.env.JMODGUARD_DIFF_BASE;
    const base = eventBase && !/^0+$/.test(eventBase) ?
        rev(eventBase) : rev(process.env.JMODGUARD_PREFLIGHT_BASE || "origin/main");
    if (eventBase && !/^0+$/.test(eventBase) && !base) {
        throw new Error("Cannot verify the CI base for the test flake checks");
    }
    const head = rev("HEAD");
    if (base && process.env.GITHUB_EVENT_NAME === "pull_request" && rev("HEAD^1") === base &&
        rev("HEAD^2") === process.env.JMODGUARD_DIFF_HEAD) return base;
    if (base && eventBase && !/^0+$/.test(eventBase) &&
        process.env.GITHUB_EVENT_NAME !== "pull_request") return base;
    if (base && base !== head) return git("merge-base", base, head).trim();
    if (head && git("status", "--porcelain", "-z")) return head;
    return rev("HEAD^") || git("hash-object", "-t", "tree", "--stdin").trim();
}

const keyOf = ({ kind, file, test }) => [kind, file, test].join("\t");

export function readAllowlist(text) {
    const entries = new Map();
    for (const row of text.split("\n")) {
        if (!row || row.startsWith("#")) continue;
        const [kind, file, test, limit, issue, extra] = row.split("\t");
        if (!["clock", "sleep", "retry"].includes(kind) || !file || !test ||
            !/^[1-9]\d*$/.test(limit) || !Number.isSafeInteger(Number(limit)) || !/^#[1-9]\d*$/.test(issue) ||
            extra !== undefined) throw new Error(`Invalid row in ${allowlistPath}: ${row}`);
        const key = keyOf({ kind, file, test });
        if (entries.has(key)) throw new Error(`Duplicate entry in ${allowlistPath}: ${key}`);
        entries.set(key, { limit: Number(limit), issue });
    }
    return entries;
}

function checkShrink(before, after) {
    const violations = [];
    for (const [key, current] of after) {
        const old = before.get(key);
        if (!old || current.limit > old.limit || current.issue !== old.issue) {
            violations.push(`${allowlistPath}: exception must shrink or stay unchanged (${key})`);
        }
    }
    return violations;
}

function repository(base) {
    const files = new Map();
    const original = new Set(git("ls-tree", "-r", "--name-only", "-z", base).split("\0"));
    const changed = git("diff", "--name-only", "-z", "--no-renames", base, "--").split("\0");
    const untracked = git("ls-files", "--others", "--exclude-standard", "-z").split("\0");
    for (const file of [...changed, ...untracked].filter(Boolean)) {
        const hunks = original.has(file) ?
            [...git("diff", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", file)
                .matchAll(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/gm)]
                .map(([, oldStart, oldCount, start, count]) => (
                    { oldStart: +oldStart, oldCount: +(oldCount ?? 1), start: +start, count: +(count ?? 1) }
                )) : [];
        files.set(file, hunks);
    }
    const before = (file) => original.has(file) ? git("show", `${base}:${file}`) : "";
    return {
        files,
        before,
        after: (file) => existsSync(file) ? readFileSync(file, "utf8") : "",
        original,
        modified(file, start, end = start) {
            return !original.has(file) || files.get(file)?.some((hunk) =>
                hunk.count && hunk.start <= end && hunk.start + hunk.count > start
            );
        },
    };
}

function frontendTest(file) {
    return /^scripts\/(?:frontend.*|administration-state|package-webui)-test\.mjs$/.test(file);
}

function runnerFile(file) {
    return file === ".config/nextest.toml" || file === "nextest.toml" ||
        /^\.github\/workflows\//.test(file) || /^scripts\/ci-[^/]*\.(?:mjs|sh)$/.test(file);
}

function lineNumber(text, index) {
    return text.slice(0, index).split("\n").length;
}

const rustUnit = { nanos: 1e-6, micros: 1e-3, millis: 1, secs: 1000, secs_f64: 1000, secs_f32: 1000, mins: 60000 };
const rustDuration = String.raw`(?:(?:std|tokio)::time::)?Duration::from_(nanos|micros|millis|secs_f64|secs_f32|secs|mins)\s*\(\s*([\d_]+(?:\.\d+)?)\s*\)`;
const budgetName = String.raw`\w*(?:timeout|deadline|time_budget)`;

function rustTestSection(file, text) {
    if (/(?:\/tests?[/.]|(?:^|\/)(?:test(?:s|_\w*)|\w+_tests?)\.rs$)/.test(file)) return () => true;
    // Repository Rust is rustfmt-formatted. Recognize inline test functions and
    // cfg(test) fixture modules by the indentation of their closing brace.
    const testAttribute = /^ *#\[(?:cfg\(test\)|(?:[\w:]+::)?test)(?:\]|\([^\n]*\)\])/;
    const followingAttribute = /^[ \t]*#\[[^\]]*\]/;
    const nextClosing = new Map();
    const nextDeclaration = [];
    const candidates = [];
    let offset = text.length + 1;
    for (const line of text.split("\n").reverse()) {
        const lineEnd = offset - 1;
        offset = lineEnd - line.length;
        const indent = /^ */.exec(line)[0].length;
        const declaration = nextDeclaration[indent];
        if (declaration && testAttribute.test(line)) {
            candidates.push({ start: offset, ...declaration });
        }
        // Within each run of attributes, remember the furthest declaration
        // reachable for each indent. A declaration may be more indented than
        // its marker, while a closing brace must have the marker's indent.
        if (!followingAttribute.test(line)) nextDeclaration.length = 0;
        if (/\b(?:mod|fn)\b/.test(line)) {
            for (let depth = nextDeclaration.length; depth <= indent; depth++) {
                nextDeclaration.push({ lineEnd, end: nextClosing.get(depth) ?? text.length });
            }
        }
        if (line[indent] === "}") nextClosing.set(indent, offset);
    }

    // A declaration consumes its line, including any potential marker there.
    const sections = [];
    let previousDeclarationEnd = -1;
    for (const { start, lineEnd, end } of candidates.reverse()) {
        if (start <= previousDeclarationEnd) continue;
        sections.push([start, end]);
        previousDeclarationEnd = lineEnd;
    }
    return (index) => sections.some(([start, end]) => index >= start && index < end);
}

export function timeoutBudgets(file, text) {
    const found = [];
    function collect(regex, classify, inScope = () => true) {
        for (const match of text.matchAll(regex)) {
            if (!inScope(match.index)) continue;
            const [name, value] = classify(match);
            const start = lineNumber(text, match.index);
            const end = start + match[0].split("\n").length - 1;
            found.push({ name, value, start, end });
        }
    }
    if (file.endsWith(".rs")) {
        const inTest = rustTestSection(file, text);
        const duration = (value, unit) => Number(value.replaceAll("_", "")) * rustUnit[unit];
        collect(new RegExp(String.raw`\b((?:[\w:]+::)?(?:timeout|recv_timeout|wait_timeout))\s*\(\s*` + rustDuration, "g"),
            ([, name, unit, value]) => [name.split("::").at(-1), duration(value, unit)], inTest);
        collect(new RegExp(String.raw`\b(?=` + budgetName + String.raw`)(\w+)[ \t]*(?::[^=\n]+)?=\s*` + rustDuration, "gi"),
            ([, name, unit, value]) => [name, duration(value, unit)], inTest);
        collect(new RegExp(String.raw`\belapsed\s*\(\s*\)\s*<=?\s*` + rustDuration, "g"),
            ([, unit, value]) => ["elapsed budget", duration(value, unit)], inTest);
    }
    if (frontendTest(file)) {
        collect(/\b(?:timeout|\w*(?:Timeout|Deadline)(?:Ms)?)\s*[:=]\s*(\d+(?:_\d+)*)(?:\s*\*\s*(\d+(?:_\d+)*))?/g,
            ([text, value, scale]) => [text.split(/[:=]/)[0].trim(), Number(value.replaceAll("_", "")) * Number((scale ?? "1").replaceAll("_", ""))]);
    }
    if (runnerFile(file)) {
        collect(/\b(period)\s*=\s*"(\d+)\s*(ms|s|m|h)"/g,
            ([, name, value, unit]) => [name, Number(value) * { ms: 1, s: 1000, m: 60000, h: 3600000 }[unit]]);
        collect(/\b(terminate-after|timeout-minutes|JMODGUARD_TEST_TIMEOUT_SEC)\s*[:=]\s*["']?(\d+)/g,
            ([, name, value]) => [name, Number(value)]);
    }
    return found;
}

function configuredRetries(file, text) {
    if (!runnerFile(file)) return [];
    const lines = [];
    for (const [index, line] of text.split("\n").entries()) {
        if (/^\s*#/.test(line)) continue;
        if (/(?:^|[\s"'=])--retries(?:[\s="'$]|$)/.test(line) && !/--retries[ =]+0\b/.test(line) ||
            /\bNEXTEST_RETRIES\s*[:=]\s*(?!["']?0\b)\S/.test(line) ||
            /^\s*retries\s*=\s*(?!0\b)\S/.test(line)) lines.push(index + 1);
    }
    return lines;
}

function contracts(repo, beforeList, afterList) {
    const errors = beforeList ? checkShrink(beforeList, afterList) : [];
    if (!existsSync(allowlistPath)) errors.push(`${allowlistPath}: retain the baseline file when it becomes empty`);
    for (const [file, hunks] of repo.files) {
        if (!file.endsWith(".rs") && !frontendTest(file) && !runnerFile(file)) continue;
        const after = repo.after(file);
        for (const line of configuredRetries(file, after)) {
            if (repo.modified(file, line)) errors.push(`${file}:${line}: nextest retries mask test failures`);
        }

        const before = timeoutBudgets(file, repo.before(file));
        const current = timeoutBudgets(file, after);
        for (const { oldStart, oldCount, start, count } of hunks) {
            if (!oldCount || !count) continue;
            const replaced = before.filter((budget) => budget.start < oldStart + oldCount && budget.end >= oldStart);
            const replacements = current.filter((budget) => budget.start < start + count && budget.end >= start);
            for (const budget of replacements) {
                const index = replaced.findIndex((old) => old.name === budget.name);
                if (index === -1) continue;
                const [old] = replaced.splice(index, 1);
                if (budget.value > old.value) {
                    errors.push(`${file}:${budget.start}: ${budget.name} increased; fix the test's synchronization`);
                }
            }
        }
    }
    return errors;
}

function finding(file, start, end, column, message) {
    const match = /^(sleep|clock|retry) in test (.*): /.exec(message);
    if (!match) throw new Error(`Unrecognized test flake diagnostic: ${message}`);
    return { file: relative(process.cwd(), file), start, end, column, message, kind: match[1], test: match[2] };
}

async function rustFindings() {
    const findings = [];
    for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
        if (!line.startsWith("{")) {
            console.log(line);
            continue;
        }
        const data = JSON.parse(line);
        if (data.reason !== "compiler-message") continue;
        const { message } = data;
        if (message.code?.code !== "test_flake") {
            if (message.rendered) process.stderr.write(message.rendered);
            continue;
        }
        const span = message.spans.find((entry) => entry.is_primary);
        if (!span) throw new Error("Rust test flake diagnostic has no primary span");
        findings.push(finding(span.file_name, span.line_start, span.line_end, span.column_start, message.message));
    }
    return findings;
}

async function eslintFindings() {
    const { ESLint } = createRequire(new URL("../tools/eslint/package.json", import.meta.url))("eslint");
    const { frontendTests } = await import("../tools/eslint/eslint.config.js");
    const lint = new ESLint({ overrideConfigFile: "tools/eslint/eslint.config.js" });
    const results = await lint.lintFiles(frontendTests);
    return results.flatMap((file) => file.messages.map((message) => {
        if (message.ruleId !== "jmodguard/test-flake") {
            throw new Error(`${file.filePath}:${message.line}: ${message.message}`);
        }
        return finding(file.filePath, message.line, message.endLine, message.column, message.message);
    }));
}

function enforce(repo, beforeList, afterList, findings) {
    const errors = [];
    const seen = new Set();
    const used = new Map();
    for (const hit of findings.sort((a, b) => a.file.localeCompare(b.file) || a.start - b.start || a.column - b.column)) {
        const key = keyOf(hit);
        // A library and its test binary can report the same source location.
        const location = `${hit.file}:${hit.start}:${hit.column}`;
        if (seen.has(`${location}\t${key}`)) continue;
        seen.add(`${location}\t${key}`);
        const diagnostic = `${location}: ${hit.message}`;
        if (repo.modified(hit.file, hit.start, hit.end)) {
            errors.push(diagnostic);
        } else if (afterList.has(key) || beforeList?.has(key)) {
            const count = (used.get(key) ?? 0) + 1;
            used.set(key, count);
            if (count > (afterList.get(key)?.limit ?? 0)) errors.push(diagnostic);
        } else if (hit.kind === "retry") {
            errors.push(diagnostic);
        } else {
            console.error(`Inherited test flake: ${diagnostic}`);
        }
    }
    return errors;
}

async function main() {
    const mode = process.argv[2];
    if (!["contracts", "rust", "eslint"].includes(mode)) throw new Error("Select contracts, rust or eslint");
    const repo = repository(baseRevision());
    const before = repo.original.has(allowlistPath) ? readAllowlist(repo.before(allowlistPath)) : undefined;
    const after = readAllowlist(repo.after(allowlistPath));
    const errors = mode === "contracts" ? contracts(repo, before, after) :
        enforce(repo, before, after, await (mode === "rust" ? rustFindings() : eslintFindings()));
    for (const error of errors) console.error(error);
    if (errors.length) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    await main();
}
