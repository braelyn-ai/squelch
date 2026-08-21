// THE AUTOMATIC THREAD STYLE, which is a guess made about somebody's mail and
// therefore the last thing that should be left to reasoning about the code.
//
// The rule is a veto chain — bubbles only when every test passes — so each test
// is asserted on its own: a fixture that would otherwise be chat, broken by one
// rule at a time. The direction of the mistakes matters more than the count of
// them. Cards for a conversation is the mail the reader has always had; chat for
// a receipt trail is a surface pretending a robot is talking to them.
//
// The quote splitter comes along because it is what feeds the length test: the
// reply that says "sounds good" under forty lines of chain is a short message,
// and a guess that measured the chain would call every real thread long.

import Foundation

@main
@MainActor
struct ThreadStyleTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        shortBackAndForthIsChat()
        nobodyRepliedIsEmail()
        aCrowdIsEmail()
        longMessagesAreEmail()
        theLineIsDrawnInBytes()
        oneHeavyMessageVetoesTheThread()
        quotedHistoryIsNotTheMessage()
        anOldDaemonKnowsNoSides()
        participationStandsAlone()
        emptyIsEmail()
        heavyIsMarkupTablesOrPictures()
        theSplitterFindsTheReply()
        theDefaultIsThreeAnswersAndTwoStyles()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - the rule, one test at a time

    /// The case the whole feature exists for: two people, short lines, both
    /// talking.
    static func shortBackAndForthIsChat() {
        equal(
            ThreadStyle.automatic([
                theirs("lunch?"), mine("yes"), theirs("1pm at the usual place"), mine("see you"),
            ]), .bubbles, "two people saying short things")
        // Two messages is already a conversation, if both sides are in it.
        equal(
            ThreadStyle.automatic([theirs("you around?"), mine("yep")]), .bubbles,
            "two is enough")
    }

    /// PARTICIPATION. A newsletter is short, it is one sender, it has no tables —
    /// and the reader has never said a word to it, which is the whole difference
    /// between mail with somebody and mail about something.
    static func nobodyRepliedIsEmail() {
        equal(
            ThreadStyle.automatic([
                theirs("today's issue: three links"), theirs("today's issue: four links"),
            ]), .classic, "nothing of the reader's own")
        // And the mirror: a thread of the reader talking to themselves has no
        // other side to draw.
        equal(
            ThreadStyle.automatic([mine("note to self"), mine("and another")]), .classic,
            "no other side either")
    }

    /// A SMALL CAST. Three voices is two people and an interloper; four is a
    /// group thread, which reads as a list of contributions and not as sides.
    static func aCrowdIsEmail() {
        equal(
            ThreadStyle.automatic([
                theirs("friday?", from: "alice@example.com"),
                theirs("works", from: "bob@example.com"),
                mine("same"),
            ]), .bubbles, "three still reads as talk")
        equal(
            ThreadStyle.automatic([
                theirs("friday?", from: "alice@example.com"),
                theirs("works", from: "bob@example.com"),
                theirs("or saturday", from: "carol@example.com"),
                mine("friday"),
            ]), .classic, "four is a group")
        // The address is the identity, and its casing is not part of it.
        equal(
            ThreadStyle.automatic([
                theirs("friday?", from: "Alice@Example.com"),
                theirs("or saturday", from: " alice@example.com "),
                mine("friday"),
            ]), .bubbles, "one person spelled two ways is one person")
    }

    /// BREVITY, ON THE MEDIAN. One long message among short ones is a chatty
    /// thread with an explanation in it; mostly long messages is correspondence.
    static func longMessagesAreEmail() {
        let essay = String(repeating: "x", count: 900)
        equal(
            ThreadStyle.automatic([theirs("hi"), mine(essay), theirs("ok"), mine("ok")]),
            .bubbles, "one essay does not make a letter")
        equal(
            ThreadStyle.automatic([theirs(essay), mine(essay), theirs("ok")]), .classic,
            "mostly essays does")
        // The line is 400 and it is a `<`: exactly at it is not under it. Held
        // in ASCII on purpose — one byte per character is what makes the
        // boundary readable as a number.
        let atTheLine = String(repeating: "x", count: ThreadStyle.chatMedianBytes)
        equal(
            ThreadStyle.automatic([theirs(atTheLine), mine(atTheLine)]), .classic,
            "the median is a floor, not a ceiling")
        equal(
            ThreadStyle.automatic([
                theirs(String(repeating: "x", count: ThreadStyle.chatMedianBytes - 1)),
                mine(String(repeating: "x", count: ThreadStyle.chatMedianBytes - 1)),
            ]), .bubbles, "and one byte under it passes")
    }

    /// THE UNIT IS BYTES, and the whole reason is mail that is not in English:
    /// a hanzi costs three bytes and carries what several English characters
    /// would, so counting graphemes read a substantial page of Chinese as a
    /// one-liner and flipped the thread to chat.
    static func theLineIsDrawnInBytes() {
        // About two hundred hanzi each: a paragraph in anybody's language, and
        // 600 bytes of it.
        let page = String(repeating: "字", count: 200)
        equal(page.count < ThreadStyle.chatMedianBytes, true, "and it counts short as characters")
        equal(
            ThreadStyle.automatic([theirs(page), mine(page), theirs(page)]), .classic,
            "a page of hanzi is a page")
        // The short exchange the feature is for, in the same script: well under
        // the 133 glyphs the line allows, so it reads as talk either way.
        equal(
            ThreadStyle.automatic([theirs("吃饭了吗"), mine("还没"), theirs("一点见")]), .bubbles,
            "and a short one is still talk")
    }

    /// ONE VETO IS THE WHOLE THREAD. A receipt or a newsletter dropped into a
    /// chatty exchange is exactly the case a bubble column makes ridiculous, so
    /// a single heavy message ends it.
    static func oneHeavyMessageVetoesTheThread() {
        var receipt = theirs("your order shipped", from: "orders@shop.example")
        receipt.htmlHeavy = true
        equal(
            ThreadStyle.automatic([theirs("did it ship?"), mine("checking"), receipt]), .classic,
            "one document among the talk")
    }

    /// THE LENGTH IS THE MESSAGE, NOT THE CHAIN. Every reply here carries the
    /// whole history under it — which is what real mail looks like — and the
    /// thread is still four short lines.
    static func quotedHistoryIsNotTheMessage() {
        let chain = (1...12).map { ">     minutes of the meeting, line \($0), at some length" }
            .joined(separator: "\n")
        let quoted = "On Tue, Aug 18, 2026 at 9:02 AM Alice <alice@example.com> wrote:\n" + chain
        let reply = { (fresh: String) in fresh + "\n\n" + quoted }

        // The fixture is only worth anything if the raw bodies WOULD have failed.
        equal(
            reply("sounds good").utf8.count > ThreadStyle.chatMedianBytes, true,
            "raw bodies are long")

        equal(
            ThreadStyle.automatic([
                body(false, reply("can you make 3pm?")),
                body(true, reply("sounds good")),
                body(false, reply("perfect, booked it")),
            ]), .bubbles, "the chain is not what was said")
    }

    /// THE OLD DAEMON. No `is_sent` on the wire means no side is known for any
    /// message, participation cannot be shown, and the whole app stays on the
    /// mail it has always drawn. Not a fallback — there is nothing to right-align.
    static func anOldDaemonKnowsNoSides() {
        let unknown = { (text: String, from: String) in
            ThreadStyle.Sample(
                fromMe: nil, freshBytes: text.utf8.count, htmlHeavy: false, sender: from)
        }
        equal(
            ThreadStyle.automatic([
                unknown("lunch?", "alice@example.com"),
                unknown("yes", "me@example.com"),
                unknown("1pm", "alice@example.com"),
            ]), .classic, "an unknown side is not the reader's")
    }

    /// THE SHORT-CIRCUIT. `participated` stands alone so the reader can ask it
    /// of `is_sent` before a single sample is built; its answer has to be the
    /// one `automatic`'s first veto would give.
    static func participationStandsAlone() {
        equal(ThreadStyle.participated([true, false]), true, "both sides spoke")
        equal(ThreadStyle.participated([true, nil]), true, "an unknown side is not the reader's")
        equal(ThreadStyle.participated([true, true]), false, "no other side")
        equal(ThreadStyle.participated([false, nil]), false, "nothing of the reader's own")
        equal(ThreadStyle.participated([]), false, "nothing at all")
    }

    static func emptyIsEmail() {
        equal(ThreadStyle.automatic([]), .classic, "nothing to read")
    }

    // MARK: - what counts as heavy

    static func heavyIsMarkupTablesOrPictures() {
        equal(ThreadStyle.htmlHeavy(html: nil, plain: "hello"), false, "plain text is never heavy")
        equal(ThreadStyle.htmlHeavy(html: "", plain: "hello"), false, "and neither is no markup")
        equal(
            ThreadStyle.htmlHeavy(
                html: "<div dir=\"ltr\">sounds good to me</div>", plain: "sounds good to me"),
            false, "a wrapper around a sentence is a sentence")
        equal(
            ThreadStyle.htmlHeavy(
                html: String(repeating: "<span style=\"color:#333\">", count: 40) + "hi",
                plain: "hi"), true, "markup dwarfing the words is a document")
        equal(
            ThreadStyle.htmlHeavy(
                html: "<TABLE><tr><td>" + String(repeating: "word ", count: 200)
                    + "</td></tr></TABLE>",
                plain: String(repeating: "word ", count: 200)), true,
            "a table is a page layout, in any casing")
        let text = String(repeating: "word ", count: 300)
        equal(
            ThreadStyle.htmlHeavy(html: "<p>\(text)</p><IMG src=a><img src=b>", plain: text), false,
            "two pictures is a signature")
        equal(
            ThreadStyle.htmlHeavy(
                html: "<p>\(text)</p><IMG src=a><img src=b><img src=c>", plain: text), true,
            "three is a layout")
        // `<img` is only a picture when the tag name ENDS there: a message that
        // talks about an image host is a message.
        equal(
            ThreadStyle.htmlHeavy(
                html: "<p>\(text) <imgur.com/a> <imgur.com/b> <imgur.com/c></p>",
                plain: text), false, "a word starting with img is not a tag")
    }

    // MARK: - the splitter that feeds the length

    static func theSplitterFindsTheReply() {
        let plain = Quotes.splitText("just the one line")
        equal(plain.visible, "just the one line", "nothing to split")
        equal(plain.quoted == nil, true, "and nothing collapsed")

        let reply = Quotes.splitText(
            "sounds good\n\nOn Tue, Aug 18, 2026 at 9:02 AM Alice <alice@example.com> wrote:"
                + "\n> the\n> chain")
        equal(reply.visible, "sounds good", "the reply is what is above the attribution")
        equal(reply.quoted?.hasPrefix("On Tue") == true, true, "the chain is what is below it")

        // A message that STARTS quoted is left whole: collapsing it would blank
        // the card, and a zero-length body would tell the guess it was chat.
        let allQuote = Quotes.splitText("> the\n> whole\n> thing")
        equal(allQuote.quoted == nil, true, "a forward is not a reply")
        equal(allQuote.visible.count > 0, true, "and it keeps its text")
    }

    // MARK: - the setting

    static func theDefaultIsThreeAnswersAndTwoStyles() {
        equal(
            ThreadStyleDefault.allCases.map(\.rawValue), ["auto", "classic", "bubbles"],
            "automatic is offered first")
        equal(ThreadStyleDefault.auto.fixed == nil, true, "automatic names no style")
        equal(ThreadStyleDefault.classic.fixed, .classic, "email names one")
        equal(ThreadStyleDefault.bubbles.fixed, .bubbles, "so does chat")
        // The stored spelling is what a shipped default reads back as; changing
        // it silently would put every reader on Automatic again.
        equal(ThreadStyleDefault(rawValue: "auto"), .auto, "the stored spelling")
        equal(ThreadStyleDefault(rawValue: "nonsense") == nil, true, "and nothing else")
    }

    // MARK: - fixtures

    static func mine(_ text: String) -> ThreadStyle.Sample {
        ThreadStyle.Sample(
            fromMe: true, freshBytes: text.utf8.count, htmlHeavy: false, sender: "me@example.com")
    }

    static func theirs(_ text: String, from: String = "alice@example.com") -> ThreadStyle.Sample {
        ThreadStyle.Sample(
            fromMe: false, freshBytes: text.utf8.count, htmlHeavy: false, sender: from)
    }

    /// A sample built the way ThreadViewer builds one: from a whole body, with
    /// the quoted history stripped out of the length, counted in utf8.
    static func body(_ fromMe: Bool, _ content: String) -> ThreadStyle.Sample {
        ThreadStyle.Sample(
            fromMe: fromMe,
            freshBytes: Quotes.splitText(content).visible.utf8.count,
            htmlHeavy: false,
            sender: fromMe ? "me@example.com" : "alice@example.com")
    }

    // MARK: - assertions

    static func equal<T: Equatable>(_ got: T, _ want: T, _ label: String, line: Int = #line) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }
}
