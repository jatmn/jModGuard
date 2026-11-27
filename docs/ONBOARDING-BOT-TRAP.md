# Onboarding bot trap

The trap catches members who select a decoy role in Discord Onboarding. It
starts disabled. When enabled, it checks whether the configured role arrived
with a join or was added in a member update. Moderators, administrators and the
server owner are always exempt.

The trap can remove selected roles from a matching member before kicking them.
It can also clean up their recent messages after Discord confirms the kick.

## Create the Anti-bot role

1. In Discord, open **Server Settings → Roles** and create **Anti-bot**.
2. Give the role zero server permissions. Check category and channel permission
   overrides, and leave the role without access grants. The setup check also
   reports individual channel grants for members with the role that the bot
   has seen. Review any member-specific grants in Discord.
3. Move **Anti-bot** below jModGuard's highest role. Give jModGuard **Kick
   Members**. jModGuard must also outrank the members it kicks.

## Add the onboarding question

1. Open **Server Settings → Onboarding → Questions**.
2. Add **Do not select this role - You will be kicked**.
3. Add a single answer named **Anti-bot role** with the description **Selecting
   this will prevent you from joining**. Set the answer to grant **Anti-bot**.
4. Turn on **Ask before a member joins** and **Allow multiple answers**. Leave
   **Make Required** off. Members can skip the question, while bots that choose
   every option will take the decoy role.
5. Publish the onboarding changes.

See the [Discord Community Onboarding FAQ](https://support.discord.com/hc/en-us/articles/11074987197975-Community-Onboarding-FAQ)
for the Community server setup and preview requirements.

## Configure and test the trap

1. In jModGuard's **Log channels** tab, set **Member kick** and **Automod** to
   staff-only channels. These logs show the kick case and the rule match.
2. Open **Automod → Onboarding bot trap** and choose **Anti-bot**. Review the
   kick action and reason. The default reason is
   `[Automod] Kicked for suspected bot (onboarding)`. Turn off any existing
   custom Honeypot role rule for **Anti-bot** so only one rule handles it.
3. Keep **Dry-run (log only)** selected, turn on **Enabled**, and save. The
   default join window is 30 minutes. Set it to 0 to check role grants at any
   time. Keeping a window helps protect members from accidental later grants.

   Leave **Roles to remove before kicking** empty to skip role removal. You
   can select up to five additional roles. The trap removes selected roles the
   member holds, in selection order. jModGuard needs **Manage Roles** and must
   outrank the member. The trap role, `@everyone`, and integration-managed
   roles are excluded. A refused or unconfirmed removal can still be followed
   by a kick if the member and rule continue to pass the safety checks.

   Set **Recent-message cleanup** to 1–60 minutes to delete messages the bot
   observed during the member's current stay and within that time before the
   kick. The default is 0 (off). Cleanup needs **Manage Messages** in each
   affected channel and uses the bot's bounded in-memory message tracking.
   In a staggered deployment, role removal and cleanup take effect after the
   running bot is upgraded.
4. In **Server Settings → Onboarding**, use **Preview** to view the server as
   a new member. Then join with a test account that has ordinary member
   permissions and select the decoy answer. Confirm the dry-run entry in
   **Automod**. Dry-run leaves the account, roles, and messages in place and
   creates no case.
5. Turn off dry-run and save. Rejoin with the test account and select the decoy
   again. Confirm the kick, the **Member kicked** log and the **Automod** entry.
   Find the kick case in the private dashboard **Cases** page and check its
   outcome and reason. Trap cases stay out of member histories and Discord
   `userinfo` case summaries.

## Troubleshooting

- If the kick fails, check **Kick Members** and move jModGuard's highest role
  above the trap role and the test account's highest role.
- If the Automod log reports skipped or unconfirmed role removals, check
  **Manage Roles**, the role hierarchy, and that the selected roles still exist.
- Keep the role exclusive to the onboarding answer. Check reaction roles,
  other bots and manual grants. A grant inside the configured join window can
  trigger the trap regardless of who applied it.
- If the trap is quiet, verify the saved role and join window, the test account's
  permissions and the configured log channels.
- If you share screenshots of your setup, redact real server, role and user IDs.
