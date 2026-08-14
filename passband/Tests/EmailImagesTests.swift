import Foundation

@main
@MainActor
struct EmailImagesTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        httpNewsletterImagesUpgradeToHTTPS()
        butterflyMXVisitorPassImagesUpgradeToHTTPS()
        httpUpgradePreservesRawURLBytes()
        explicitNonstandardHTTPPortsArePreserved()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    private static func httpNewsletterImagesUpgradeToHTTPS() {
        let source = "http://cdn.example.com/newsletter/hero.jpg?x=1&amp;y=2"
        let result = ImageProxy.rewrite("<img src=\"\(source)\">")
        equal(
            result.urls, ["https://cdn.example.com/newsletter/hero.jpg?x=1&y=2"],
            "http image upgrades and entities decode")
        equal(result.html.contains("http://cdn.example.com"), false, "old scheme is absent")
        let proxy = result.html.firstMatch(of: /passband-img:\/\/[^\"]+/).map { String($0.0) }
        equal(
            proxy.flatMap { URL(string: $0) }.flatMap(ImageProxy.original), result.urls[0],
            "signed proxy recovers upgraded target")
    }

    private static func explicitNonstandardHTTPPortsArePreserved() {
        let source = "http://images.example.com:8080/art.png"
        let result = ImageProxy.rewrite("<img src='\(source)'>")
        equal(result.urls, [source], "nonstandard port is not upgraded")
    }

    private static func butterflyMXVisitorPassImagesUpgradeToHTTPS() {
        let source =
            "http://cdn.mcauto-images-production.sendgrid.net/45baeb93b260a71d/"
            + "f4fc2026-7d35-498e-b450-cff243205ff2/1000x250.png"
        let result = ImageProxy.rewrite(
            "<img class=\"max-width\" width=\"165\" src=\"\(source)\" height=\"41\">")
        equal(
            result.urls, [source.replacingOccurrences(of: "http://", with: "https://")],
            "ButterflyMX SendGrid image upgrades")
    }

    private static func httpUpgradePreservesRawURLBytes() {
        for path in ["100%discount.png", "ünïcode.png", "p|q.png"] {
            let result = ImageProxy.rewrite("<img src=\"http://h/\(path)\">")
            equal(result.urls, ["https://h/\(path)"], "scheme splice preserves \(path)")
        }
    }

    private static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        guard got != want else { return }
        failures += 1
        print("line \(line): \(label)\n  got:  \(got)\n  want: \(want)")
    }
}
