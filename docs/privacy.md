# Privacy Policy

`ani-gui` is an open-source, local-only desktop application. It does not operate a backend server and does not collect telemetry, analytics, or personal data.

## Third-party accounts (MyAnimeList & AniList)

When you choose to connect a MyAnimeList or AniList account in **Settings → Accounts**:

- **Authentication:** Sign-in happens directly between `ani-gui` on your machine and MyAnimeList (`myanimelist.net` / `api.myanimelist.net`) or AniList (`anilist.co` / `graphql.anilist.co`). No third-party relay server is involved.
- **Credentials & tokens:** OAuth tokens are stored exclusively in your operating system's keychain (Secret Service on Linux; Keychain on macOS) and are never written to `ani-gui`'s database or config files.
- **List data:** Your imported anime list, watch progress, and pending updates are stored locally in `~/.local/share/ani-gui/ani-gui.db` (see [Where your data lives](data.md)) so the app can display your list and sync progress when you finish episodes or submit ratings.
- **Disconnecting & deletion:** Pressing **Disconnect** in Settings deletes the stored token from your system keychain and removes the imported list and queued updates from `ani-gui`'s local database. Your remote MyAnimeList or AniList account is left untouched. You can also revoke `ani-gui`'s access at any time from your MyAnimeList or AniList account settings.

## Contact

Questions or issues can be reported on the [GitHub repository](https://github.com/SweetTooth05/ani-gui/issues).
