# Connecting MyAnimeList and AniList

Connect either account, or both, in **Settings → Accounts**. ani-gui then:

- imports your anime list when you connect, when the app starts (at most every
  half hour) and when you press **Sync now**, and shows it on My List grouped
  by status. Episodes up to your list's progress count as watched on show
  pages, and shows you're watching appear in Continue Watching;
- updates your progress on each connected service when you finish an episode
  (marking the show completed after its final episode), and offers a 1–10
  rating then and in the show page's **Your list** section;
- keeps changes it couldn't send (offline, server down) and retries them on the
  next sync. When both lists have the same show, the most recently updated one
  wins.

Sign-in tokens are stored only in your system keychain (Secret Service, such as
GNOME Keyring or KWallet, on Linux; the Keychain on macOS), never in ani-gui's
files. Without a working keychain, connecting fails with an explanation rather
than saving tokens anywhere else. See also the [privacy policy](privacy.md).

## MyAnimeList

Official releases ship with a built-in MyAnimeList Client ID:

1. Open **Settings → Accounts → MyAnimeList** and press **Connect**.
2. Approve access in the browser that opens. ani-gui listens on
   `127.0.0.1:53682` for up to five minutes to receive the redirect; when the
   page says you can close the tab, you're connected.

The sign-in uses OAuth 2 with PKCE, and the token refreshes itself. If port
53682 is taken by another program, close it and try again.

### Building from source or using your own Client ID

Source builds don't include a built-in Client ID (unless `ANI_GUI_MAL_CLIENT_ID`
is set at compile time). On official builds, you can also click **Use custom
Client ID** to use your own app instead:

1. Sign in at [myanimelist.net/apiconfig](https://myanimelist.net/apiconfig)
   and choose **Create ID**.
2. Set **App Type** to **other**, and **App Redirect URL** to exactly:
   ```
   http://localhost:53682/callback
   ```
   Fill in the other required fields (name, description, homepage) however you
   like.
3. Copy the **Client ID** (not the secret: "other" apps don't have one) into
   Settings → Accounts → MyAnimeList, and press **Connect**.

## AniList

1. Go to [anilist.co/settings/developer](https://anilist.co/settings/developer)
   and choose **Create New Client**.
2. Set the **Redirect URL** to exactly:
   ```
   https://anilist.co/api/v2/oauth/pin
   ```
3. Copy the client's **ID** (a number) into Settings → Accounts → AniList, and
   press **Connect**.
4. Approve access in the browser. AniList then shows a token: copy all of it,
   paste it into the field ani-gui shows, and press **Save token**.

AniList tokens last a year. When one expires or you revoke access, ani-gui
marks the account disconnected and asks you to connect again.

**Disconnect** removes the token from the keychain and forgets the imported
list; your lists on MyAnimeList and AniList are untouched.

