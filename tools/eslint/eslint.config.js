/**
 * Host-only ESLint config for jModGuard browser scripts.
 * Paths are relative to the repository root.
 */
import js from "@eslint/js";
import { defineConfig } from "eslint/config";
import globals from "globals";

export default defineConfig([
    {
        files: [
            "frontend/**/*.js",
        ],
        extends: [js.configs.recommended],
        languageOptions: {
            ecmaVersion: 2022,
            sourceType: "script",
            globals: {
                ...globals.browser,
            },
        },
        rules: {
            // catch (_) {} and unused parameters are intentional on existing pages.
            // imageTestConfigured is destructured out in scripts/frontend-state-test.mjs.
            "no-unused-vars": ["error", {
                args: "none",
                caughtErrors: "none",
                varsIgnorePattern: "^imageTestConfigured$",
            }],
            "no-empty": ["error", { allowEmptyCatch: true }],
            "preserve-caught-error": "off",
        },
    },
]);
