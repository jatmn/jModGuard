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
            "crates/conductor/frontend/**/*.js",
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
            // Existing pages use catch (_) {} and unused parameters on purpose.
            "no-unused-vars": ["error", {
                args: "none",
                caughtErrors: "none",
                varsIgnorePattern: "^(esc|imageTestConfigured)$",
            }],
            "no-empty": ["error", { allowEmptyCatch: true }],
            "no-useless-assignment": "off",
            "preserve-caught-error": "off",
        },
    },
]);
