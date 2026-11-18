/**
 * Host-only ESLint config for jModGuard browser scripts.
 * Paths are relative to the repository root.
 */
import js from "@eslint/js";
import { defineConfig } from "eslint/config";
import globals from "globals";
import testFlake from "./test-flake.js";

export const frontendTests = [
    "scripts/frontend*-test.mjs",
    "scripts/administration-state-test.mjs",
    "scripts/package-webui-test.mjs",
];

export default defineConfig([
    {
        files: [
            "frontend/**/*.js",
        ],
        extends: [js.configs.recommended],
        languageOptions: {
            ecmaVersion: 2022,
            sourceType: "module",
            globals: {
                ...globals.browser,
            },
        },
        rules: {
            // catch (_) {} and unused parameters are intentional on existing pages.
            "no-unused-vars": ["error", {
                args: "none",
                caughtErrors: "none",
            }],
            "no-empty": ["error", { allowEmptyCatch: true }],
            "preserve-caught-error": "off",
        },
    },
    {
        files: frontendTests,
        linterOptions: { noInlineConfig: true },
        languageOptions: { globals: globals.node },
        plugins: { jmodguard: { rules: { "test-flake": testFlake } } },
        rules: { "jmodguard/test-flake": "error" },
    },
]);
