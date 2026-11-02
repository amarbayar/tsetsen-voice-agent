# Security

Tsetsen listens to your voice, reads your screen and can act on your behalf, so security
reports are taken seriously.

**Please don't open a public issue for a vulnerability.** Report it privately through GitHub's
[private vulnerability reporting](../../security/advisories/new) on this repository. Include what
an attacker can do, and the steps to reproduce it.

Especially in scope:

- Anything that lets text from a web page, email, document or app trigger or approve an action.
- Data leaving the device against the egress policy, or content (audio, transcripts, screen
  text) reaching logs.
- Credential handling: OAuth tokens, keychain storage, API keys.
- Opening, launching or sending something the user did not ask for.

Tsetsen is pre-alpha, and only the latest `main` is supported.
