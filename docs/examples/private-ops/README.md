# Private ops examples

Copy these into a **private** operations repository. Bunny Storage passwords
and account API keys must not live in this public project.

Live-test learnings that belong in every Bunny install (not one
environment’s hostnames or IDs) are in
[DEPLOY-BUNNY.md](https://github.com/jatmn/jModGuard/blob/main/docs/DEPLOY-BUNNY.md)
(that URL still works after you copy this folder into a private ops repo).
Short version:

- Ops **polls** jModGuard. A push to the ops repo does not deploy.
- Origin-pull `/api` with **OriginUrl** to the API endpoint Pull Zone host.
  Do not use OriginMagicContainers on a Storage-origin Pull Zone.
- At most five URL patterns per edge-rule trigger. Disable Cookies must be
  off. Do not paste Magic Container GET/PATCH JSON (it dumps env).
- The Bunny API image does not serve `/` (a default local build may).
  Probe `/health`.
- `JMODGUARD_WEB_BASE_URL` and the Discord redirect must be the hostname
  people type (Force SSL on a custom hostname).

[publish-frontend.yml](publish-frontend.yml) is the Storage/CDN publisher.
Cron skips an upload only when this git tree was already published **and**
Storage still matches those three files. **Run workflow** always uploads: use
it after a hand edit or delete on Storage, or when a bad UI is live (set
`ref` to a known-good tag or SHA and confirm the purge). Disabling the
workflow does not roll Storage back.
