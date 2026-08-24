# Changelog

<!-- GENERATED FILE. Every note here is written in
     passband/Sources/Passband/Lib/ReleaseNotes.swift, which is what
     the app's own What's New card reads. Edit that table and re-run
     passband/make-changelog.sh; an edit made here is lost on the
     next release. -->

What each version of Passband brought, in the app and in the daemon
behind it. The two ship separately: the app updates itself, and the
daemon is rolled onto hosted accounts or pulled as an image on a
self-host box, so every note says which one it landed in.

## 0.0.4 (2026-08-24)

Forwarding, reminders, and mail that renders like its sender meant it to.

### App

- Forward what you are reading. The f key in the reader opens a composer with the original already in it, quote and attachments and all, and shows you exactly what is going along before you send it.
- Park an email until later. The h key asks when, in the words you would use ("next tuesday", "the 24th"), and the thread comes back at the top of your board on the day you named.
- Embedded images render where their sender put them, instead of collecting at the bottom as attachments.
- The window reads as one bar. The email's subject sits up beside the traffic lights, and the mail bar stops shuffling when you switch pages.
- Unsubscribing or blocking a sender closes the email and moves you on, rather than leaving you holding the thing you just got rid of.
- The banking card clears once you have actually seen it, and stays cleared across a restart.
- The s key in the reader searches everything from that sender.
- This card. New versions say what they brought, once, and never again.

### Daemon

- Senders writing in Chinese, Japanese or Korean arrive as their names instead of a row of question marks.
- Login codes and 2FA mail stay sealed even when they are worded oddly enough to dodge the usual patterns.
- A triage outage no longer costs you mail. Rows stay queued when the model is unreachable and get judged when it comes back, instead of being filed on a guess.
- Reminders and forwarding are served by the daemon, so both work from any client you have paired.

## 0.0.3 (2026-08-18)

Updates that take one click, and a reader that holds still.

### App

- A new version arrives as a card in the window with a single Update button that installs it and relaunches, instead of two dialogs asking the same question twice.
- Attachments open in Quick Look, the same panel the Finder uses. Photos render in the column and open on a click.
- A thread reads oldest to newest with the newest parked at the top, and the rail beside it holds still while you scroll.
- Settings stamps the version in the corner, selectable, because that is the first thing any bug report asks for.

### Daemon

- Triage verdicts expire, so a sender you have since taught it about is judged again rather than on a months-old opinion.
- A pattern match no longer decides a tier by itself, and asking for a second opinion buys the model more context rather than just more time.
- Asking for a re-triage outranks the age cutoff, so an old thread can still be reconsidered.

## 0.0.2 (2026-08-14)

Packages that track themselves, and a gate that asks the right question first.

### App

- The connect screen asks where your mail should run before it asks for a credential, so the answer you give matches the install you have.
- Newsletter images load over plain HTTP, which is where a surprising amount of newsletter art still lives.
- The shipment card says what the carrier said, not just what the email claimed.

### Daemon

- Package tracking talks to four carriers directly, and a shipment goes quiet seven days after it lands.
- A failed sign-in no longer forfeits the message it was working on.

## 0.0.1 (2026-08-12)

The first build.

### App

- Passband, on your desk.

### Daemon

- squelchd, triaging your mail on a machine you control.

