# Elizabeats

Elizabeats is a DJ-operated radio playout system. It lets a DJ assemble and run a program from searchable audio without automating the choice of what airs.

## Language

**Track**: A playable audio item selected from either the station's local library or YouTube Music. Tracks may represent music, station IDs, jingles, or other recorded material. _Avoid_: Song, cart, audio file

**Break**: A silent Queue item that, when Playing, prevents automatic advancement until the DJ manually skips it. A Break represents time in which the DJ expects to speak or use another input on the mixing board. _Avoid_: Stop marker, talk track

**Queue**: The ordered Tracks and Breaks deliberately assembled by the DJ for the current Session. It is the heart of the application: playback, preparation, recovery, and logging follow from Queue transitions. An empty Queue is never populated automatically. _Avoid_: Playlist, schedule, rotation

**Upcoming Item**: A Queue item that has not begun Playing. An Upcoming Track may still be preparing, while an Upcoming Break is immediately ready. _Avoid_: Queued Queue item

**Active Item**: A Playing Queue item that occupies the front of the Queue and receives transport controls. During a crossfade, the incoming Track may already be Playing, but it does not become the Active Item until the outgoing Track leaves. _Avoid_: Current song, now playing item

**Incoming Track**: A prepared Track that is Playing on the second audio deck while crossfading in behind the Active Item. It occupies the second position in the Queue and becomes the Active Item when the outgoing Track leaves. _Avoid_: Next Track

**Session**: The current working period represented by the recoverable Queue. A clean launch or choosing Start Empty begins a new Session with an empty Queue; recovering an interrupted Queue continues its Session. _Avoid_: Playout Session, show, broadcast

**Play Log**: The persistent history of Tracks and Breaks whose Playing state actually began. It records what Elizabeats played or stopped for, not what was merely added to the Queue. _Avoid_: Queue history, playlist history

**Library**: The combined searchable collection of local Tracks and YouTube Music Tracks available to add to the Queue. Matching local Tracks take priority in search results. _Avoid_: Catalog, music folder

**Download Cache**: The durable, size-limited collection of YouTube Music audio that Elizabeats retains for reuse across Sessions. A cached Track remains part of the YouTube Music Source rather than becoming a local Track. _Avoid_: Download library, saved library, offline library

**Source**: The origin of a Track, either YouTube Music or the station's local library. Each Source assigns its own Track identities. _Avoid_: Service, backend, provider
