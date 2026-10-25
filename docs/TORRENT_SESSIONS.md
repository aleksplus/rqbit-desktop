❯ @crates explain how to access tracker status and torrent registered state     
  using rqbit rust api                                                          
  ⎿  Listed directory crates/

  Thought for 53s, searched for 14 patterns, read 6 files, listed 2 directories (ctrl+o to expand)

● Here's how the pieces fit together, based on the current code.

  1. Registered torrents: Session is the registry

  All torrents live in the session. Two accessors
  (crates/librqbit/src/session.rs):

  - Session::get(id) — session.rs:1425, looks up one torrent by TorrentIdOrHash
    (numeric TorrentId or 20-byte Id20 info hash)
  - Session::with_torrents(cb) — session.rs:1089, snapshots all registered
    torrents under a read lock:

  use librqbit::session::TorrentIdOrHash;

  // list all
  session.with_torrents(|torrents| {
      for (id, handle) in torrents {
          println!("{id}: {} {:?}", handle.info_hash(), handle.name());
      }
  });

  // one torrent by id or hash
  let handle = session
      .get(TorrentIdOrHash::parse("6a2a1c...")?)   // or From<TorrentId>
      .expect("torrent not registered");

  There's also the higher-level librqbit::Api wrapper
  (crates/librqbit/src/api.rs) — this is what the desktop GPUI client and HTTP
  API use. It gives you serializable responses:

  let api = Api::new(session.clone(), None);

  let list = api.api_torrent_list_ext(ApiTorrentListOpts { with_stats: true });
  // TorrentListResponse
  let handle = api.mgr_handle(idx)?;                    // -> 
  ManagedTorrentHandle
  let details = api.api_torrent_details(idx)?;          // files, output folder,
  etc.
  let stats = api.api_stats_v1(idx)?;                   // TorrentStats

  2. Lifecycle state: ManagedTorrent

  Each registered torrent is an Arc<ManagedTorrent>
  (crates/librqbit/src/torrent_state/mod.rs). The state machine is
  ManagedTorrentState (mod.rs:67): Initializing → Paused → Live, with Error
  reachable from initializing/live.

  use librqbit::torrent_state::ManagedTorrentState;

  handle.with_state(|state| match state {
      ManagedTorrentState::Initializing(_) => { /* checking existing files */ }
      ManagedTorrentState::Paused(_)       => { /* not running */ }
      ManagedTorrentState::Live(live)      => { /* active, Arc<TorrentStateLive>
  */ }
      ManagedTorrentState::Error(e)        => { /* e: &anyhow::Error */ }
      ManagedTorrentState::None            => unreachable!("internal swap 
  state"),
  });

  Convenience accessors on the handle:

  ┌─────────────────────────────┬──────────────┬───────────────────────────┐
  │           Method            │     Line     │          Returns          │
  ├─────────────────────────────┼──────────────┼───────────────────────────┤
  │                             │              │ Option<Arc<TorrentStateLi │
  │ handle.live()               │ mod.rs:285   │ ve>> (peers, speed        │
  │                             │              │ estimators, haves)        │
  ├─────────────────────────────┼──────────────┼───────────────────────────┤
  │ handle.is_paused()          │ mod.rs:475   │ bool                      │
  ├─────────────────────────────┼──────────────┼───────────────────────────┤
  │ handle.stats()              │ mod.rs:508   │ TorrentStats — see below  │
  ├─────────────────────────────┼──────────────┼───────────────────────────┤
  │ handle.wait_until_initializ │ mod.rs:572,5 │                           │
  │ ed() /                      │ 96           │ futures for async waiting │
  │ wait_until_completed()      │              │                           │
  └─────────────────────────────┴──────────────┴───────────────────────────┘

  handle.stats() is the polling-friendly path — it flattens the state machine
  into TorrentStatsState (Initializing { paused } / Live / Paused / Error) plus
  progress bytes, speeds, per-file progress, and error string
  (torrent_state/stats.rs:48). The CLI's status loop uses exactly this
  (crates/rqbit/src/main.rs:1181):

  let stats = handle.stats();
  match stats.state {
      TorrentStatsState::Live => {
          let live = handle.live().unwrap();
          let down = live.down_speed_estimator();
          // stats.progress_bytes / stats.total_bytes, stats.finished, ...
      }
      TorrentStatsState::Initializing { paused } => { /* stats.progress_bytes */
  }
      _ => {}
  }

  3. Tracker status — partially exposed, be aware

  This is the caveat: rqbit does not expose live per-tracker status (no "last
  announce result / next announce in Xs / peers returned" like qBittorrent has).
  What exists:

  What you can read today — the tracker URL list, a public field on the shared
  part of the torrent:

  // HashSet<url::Url> — merged from the .torrent/magnet + session-level 
  SessionOptions::trackers
  for url in handle.shared().trackers {
      println!("tracker: {url}");
  }

  Session-wide extras are on SessionOptions::trackers and disable_trackers
  (session.rs:299-300, merged in make_peer_rx, session.rs:1568-1580).

  Why there's nothing more: when a torrent goes live, Session::make_peer_rx
  (session.rs:1586) calls TrackerComms::start
  (crates/tracker_comms/src/tracker_comms.rs:143), which spawns one background
  task per tracker (HTTP or UDP). Each task announces on its interval and pushes
  discovered peer addresses into the merged peer stream. Announce results,
  retries, and errors never leave those tasks — they're only emitted as
  debug!/trace! tracing events ("error calling tracker: {err:#}",
  tracker_comms.rs:252). The TrackerCommsStats/TorrentStatsProvider types you'll
  see in that crate flow the other direction: they feed download progress to
  trackers for announces.

  If your UI needs real tracker status (relevant for feature/web), you'd extend
  TrackerComms: have each per-tracker task write into an Arc<Mutex<HashMap<Url, 
  TrackerStatus>>> (last error, last success time, interval, peers received)
  created in TrackerComms::start and exposed alongside the returned peer stream,
  then surface it through ManagedTorrentShared or Api::api_torrent_details. As
  it stands, the only observable signals are the tracing logs and the peers
  arriving on the stream.

✻ Sautéed for 1m 27s · done 6:07 PM