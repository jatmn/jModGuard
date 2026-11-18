const timerModules = new Set(["node:timers", "timers", "node:timers/promises", "timers/promises"]);
const testModules = new Set(["node:test"]);
const loopTypes = new Set(["ForStatement", "ForInStatement", "ForOfStatement", "WhileStatement", "DoWhileStatement"]);

function property(node, name) {
    return node?.type === "MemberExpression" && (
        node.computed ? node.property.value === name : node.property.name === name
    );
}

function binding(source, node) {
    if (node?.type !== "Identifier") return undefined;
    for (let scope = source.getScope(node); scope; scope = scope.upper) {
        if (scope.set.has(node.name)) return scope.set.get(node.name);
    }
    return undefined;
}

function globalName(source, node, name) {
    return node?.type === "Identifier" && node.name === name && !binding(source, node)?.defs.length;
}

function globalMember(source, node, name) {
    return property(node, name) && ["globalThis", "global", "window"].some((object) =>
        globalName(source, node.object, object)
    );
}

function importDefinition(source, node, modules) {
    const [def] = binding(source, node)?.defs ?? [];
    return def?.type === "ImportBinding" && modules.has(def.parent.source.value) ? def.node : undefined;
}

function hostTimer(source, node) {
    if (globalName(source, node, "setTimeout") || globalMember(source, node, "setTimeout")) return true;
    const imported = importDefinition(source, node, timerModules);
    if (imported?.imported?.name === "setTimeout") return true;
    const namespace = property(node, "setTimeout") && importDefinition(source, node.object, timerModules);
    return namespace?.type === "ImportNamespaceSpecifier" || namespace?.type === "ImportDefaultSpecifier";
}

function clockControl(source, node) {
    const timers = node?.object;
    if (!property(timers, "timers")) return false;
    const mock = timers.object;
    if (importDefinition(source, mock, testModules)?.imported?.name === "mock") return true;
    if (!property(mock, "mock")) return false;
    const [def] = binding(source, mock.object)?.defs ?? [];
    return def?.type === "Parameter" && def.node.parent?.type === "CallExpression" &&
        def.node.parent.arguments.includes(def.node) &&
        isTest(source, def.node.parent.callee);
}

function isTest(source, callee) {
    const imported = importDefinition(source, callee, testModules);
    return imported?.type === "ImportDefaultSpecifier" || ["test", "it"].includes(imported?.imported?.name) ||
        (["test", "it"].some((name) => property(callee, name)) &&
            importDefinition(source, callee.object, testModules)?.type === "ImportNamespaceSpecifier");
}

function clockIsMocked(source, node, api) {
    // Only a preceding, unconditional setup call in an enclosing block can
    // establish this clock. Installing one in a sibling test has no effect here.
    let current = node;
    for (let parent = node.parent; parent; current = parent, parent = parent.parent) {
        if (parent.type !== "BlockStatement" && parent.type !== "Program") continue;
        const index = parent.body.indexOf(current);
        for (let i = index - 1; i >= 0; i--) {
            const statement = parent.body[i];
            const call = statement.type === "ExpressionStatement" && statement.expression;
            if (call?.type !== "CallExpression" || !clockControl(source, call.callee)) continue;
            if (property(call.callee, "reset")) return false;
            if (!property(call.callee, "enable")) continue;
            if (!call.arguments.length) return true;
            const apis = call.arguments[0]?.properties?.find((entry) => entry.key?.name === "apis");
            return apis?.value?.elements?.some((element) => element?.value === api) ?? false;
        }
    }
    return false;
}

function testName(source, node) {
    let helper;
    for (let parent = node.parent; parent; parent = parent.parent) {
        if (parent.type === "CallExpression" && isTest(source, parent.callee)) {
            return parent.arguments[0]?.value ?? "<unnamed test>";
        }
        if (parent.type === "FunctionDeclaration") helper ??= parent.id.name;
        if (parent.type === "VariableDeclarator" && parent.id.type === "Identifier" &&
            ["ArrowFunctionExpression", "FunctionExpression"].includes(parent.init?.type)) helper ??= parent.id.name;
    }
    return helper ?? "<top level>";
}

function assertion(source, callee) {
    const candidate = callee.type === "MemberExpression" ? callee.object : callee;
    const [def] = binding(source, candidate)?.defs ?? [];
    return def?.type === "ImportBinding" && /^(?:node:)?assert(?:\/strict)?$/.test(def.parent.source.value);
}

export default {
    meta: {
        type: "problem",
        schema: [],
        messages: {
            sleep: "sleep in test {{test}}: wait on a signal or use fake timers",
            clock: "clock in test {{test}}: inject a fixed clock or mock Date",
            retry: "retry in test {{test}}: let the first failed assertion fail the test",
        },
    },
    create(context) {
        const source = context.sourceCode;
        const report = (node, messageId) => context.report({
            node, messageId, data: { test: testName(source, node) },
        });
        const hostDate = (node) => globalName(source, node, "Date") || globalMember(source, node, "Date");
        return {
            CallExpression(node) {
                const { callee } = node;
                if (hostTimer(source, callee) && !clockIsMocked(source, node, "setTimeout")) {
                    report(node, "sleep");
                }
                if ((hostDate(callee) || (property(callee, "now") && hostDate(callee.object))) &&
                    !clockIsMocked(source, node, "Date")) {
                    report(node, "clock");
                }
                if (!assertion(source, callee)) return;
                const ancestors = source.getAncestors(node);
                const loop = ancestors.findLastIndex((ancestor) => loopTypes.has(ancestor.type));
                if (loop >= 0 && ancestors.slice(loop + 1).some((ancestor, index, inner) =>
                    ancestor.type === "TryStatement" && ancestor.handler && ancestor.block === inner[index + 1]
                )) {
                    report(node, "retry");
                }
            },
            NewExpression(node) {
                if (!node.arguments.length && hostDate(node.callee) && !clockIsMocked(source, node, "Date")) {
                    report(node, "clock");
                }
            },
        };
    },
};
