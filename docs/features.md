# Features

- **Home**: a hero banner, Continue Watching, recommendations based on what you
  watch, and AniList's trending shows. Opens from the local cache straight away
  and refreshes in the background.
- **Search** opens on a ready-made listing (Trending now, Popular this season,
  Upcoming, All-time popular or Top rated). Typing a title searches AllAnime,
  with sub/dub episode counts. A row of **filter dropdowns** searches AniList's
  catalogue the way AniList and MyAnimeList's browse pages do: genres and tags
  (audience such as Shounen or Seinen, and themes) to include (green, +) or
  leave out (red, −), formats, airing status, season, year, source material,
  country, minimum score, length, and sort order. Active genre and tag picks
  show as removable pills under the row.
- **Show pages** with the synopsis, score, and an episode list that marks what
  you've watched. Genres and AniList tags (with how strongly each applies)
  appear as chips; click one to list every show with it. Spoiler tags stay
  hidden until you ask for them.
- **In-window playback** through libmpv: seek bar, volume, fullscreen, external
  subtitle and audio tracks, autoplay of the next episode, and resume from where
  you stopped. **Skip intro** and **Next episode** appear only during the
  opening and ending (timestamps from AniSkip, or the video's chapters), and the
  controls hide after a moment without mouse movement.
- **My List** for the shows you want to keep track of, plus your
  MyAnimeList and/or AniList list grouped by status.
- **MyAnimeList and AniList sync** (optional): imports what you've watched,
  marks episodes you finish, and sends a 1–10 rating when you finish a show.
  See [Connecting MyAnimeList and AniList](accounts.md).
- **Calendar** of this week's airing episodes, from AniList.
- **Downloads** manager with live progress, cancel and retry.
- **Shares history with ani-cli**: what you watch here shows up in
  `ani-cli -c`, and the other way round.
- **Settings**:
  - Appearance: theme (Night, Midnight, Dusk, Daylight), accent colour (Amber,
    Coral, Violet, Cyan, Mint), English or romaji titles, 24- or 12-hour clock,
    the page ani-gui opens on, and poster hover details.
  - Playback: sub/dub, quality, autoplay, the Up next countdown, skipping
    openings automatically, the skip step, and keeping the screen awake.
  - Browsing: what Search opens with, and whether to show spoiler tags.
  - Download folder, updates, accounts, a Diagnostics panel that checks your
    dependencies, and About.

## Player keys

| Key | Action |
|---|---|
| Space | Play / pause |
| ← / → | Seek back / forward |
| ↑ / ↓ | Volume |
| M | Mute |
| F | Fullscreen |
| N | Next episode |
| Esc | Leave fullscreen, then the player |
| Alt+← | Back (anywhere in the app) |
| F11 | Fullscreen (anywhere in the app) |
| Ctrl+W / Ctrl+Q | Close the window / quit (Linux) |

