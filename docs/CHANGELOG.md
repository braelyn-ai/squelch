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

## 0.0.5 (2026-08-25)

Groups you can address as one, and a mailbox that says so when it stops working.

### App

- Address a group by name. Groups sits at the bottom of the rail on Cmd-6, holds a named audience, and a Groups button beside the To line puts one into an email.
- How a group is addressed is settled once, when you make it: everybody on the To line, everybody blind, or one separate email per person. The review before you send says which of the three you are about to do.
- A group shows what has already gone to it, including the mail you sent those people before the group existed.
- The composer has a Bcc row, on the messages you want one on.
- When your mailbox stops working, Passband says so. An expired Google sign-in used to look exactly like nobody writing to you; now a banner says what broke and since when, and on a hosted account it carries the link that repairs it.
- Re-triage runs in front of you with the queue counting down, instead of a toast over a board still showing the old verdicts.
- One click on a row in Auth opens the mail the code arrived in.
- Shipment cards read as one line, and asking one to check now no longer takes the app down with it.
- The assistant's Opus setting is Opus 5, and a preference saved on the older model moves forward by itself.
- Inviting a friend hands them the waitlist for now. An invite cannot be redeemed until Google's review clears, so the sheet says so rather than minting a code that would bounce.

### Daemon

- Sending to a group one person at a time happens in the background, so a twelve person list goes out without the send timing out, and the Groups page watches it happen.
- A send that reached some of a group and not the rest says so, and says how many of each.
- Blind copies go out blind and are still recorded, so a bcc-only send lists in your sent mail with everyone it actually reached.
- A model outage no longer costs you the rest of the day. A call the gateway turns away for free is refunded to your triage budget, instead of burning the day's allowance in minutes and leaving mail unjudged until midnight.
- The assistant answers again on hosted accounts, whichever model you picked for it.
- A hosted mailbox that lost its Google consent is reconnected by the person who owns it, from the banner, rather than by somebody reaching into the cluster on their behalf.
- Your daemon notices the moment a mailbox loses its sign-in and remembers since when, which is what the banner in the app is reading.

## 0.0.4 (2026-08-24)

Forwarding, reminders, invites, and mail that renders like its sender meant it to.

### App

- Forward what you are reading. The f key in the reader opens a composer with the original already in it, quote and attachments and all, and shows you exactly what is going along before you send it.
- Park an email until later. The h key asks when, in the words you would use ("next tuesday", "the 24th"), and the thread comes back at the top of your board on the day you named.
- Invite a friend. The share sheet writes the mail, shows you exactly what is going out, and sends it from your own mailbox under your own name.
- Threads read the way you want them to. Email cards, chat bubbles, or Automatic, which picks per thread from how the conversation actually reads.
- Embedded images render where their sender put them, instead of collecting at the bottom as attachments.
- Notification banners carry the sender's own mark, a brand's logo or a correspondent's initials, so you know who wrote before you open it.
- The window reads as one bar. The email's subject sits up beside the traffic lights, and the mail bar stops shuffling when you switch pages.
- A long thread scrolls without stutter, however far back it goes.
- Unsubscribing or blocking a sender closes the email and moves you on, rather than leaving you holding the thing you just got rid of.
- The banking card clears once you have actually seen it, and stays cleared across a restart.
- The s key in the reader searches everything from that sender.
- This card. New versions say what they brought, once, and never again.

### Daemon

- Senders writing in Chinese, Japanese or Korean arrive as their names instead of a row of question marks.
- Login codes and 2FA mail stay sealed even when they are worded oddly enough to dodge the usual patterns.
- A triage outage no longer costs you mail. Rows stay queued when the model is unreachable and get judged when it comes back, instead of being filed on a guess.
- Reminders and forwarding are served by the daemon, so both work from any client you have paired.
- New mail reaches the screen in seconds. Gmail is polled every five seconds rather than every forty-five, so a login code is in front of you about as fast as it arrives.
- An invite you send goes out through your own Gmail, and the address you sent it to never leaves your machine.

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

