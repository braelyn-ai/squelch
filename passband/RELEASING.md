# Releasing Passband

Push a tag; CI does the rest:

```sh
# bump VERSION (and its project.yml mirror), commit, then:
git tag "passband-v$(cat VERSION)" && git push origin "passband-v$(cat VERSION)"
```

`.github/workflows/passband-release.yml` builds, signs, notarizes, publishes
the GitHub release, regenerates the appcast (committed to main, which
redeploys the site), and bumps the Homebrew cask. It needs five repo
secrets, listed at the top of the workflow file. The daemon's bare `v*`
tags are a different workflow; the two never overlap.

## Local fallback

The same release can be cut from this machine with one command:

```sh
./release.sh        # or --dry to rehearse without publishing
```

It runs the steps below, which are also what CI does — kept documented for
when a step needs doing by hand. Prerequisites on the release machine:
the Developer ID certificate, the `passband-notary` notarytool profile
(see build-release.sh), and the Sparkle EdDSA private key in the login
keychain (created by `vendor/Sparkle/bin/generate_keys`; back it up with
`generate_keys -x sparkle-private-key.txt` and store it somewhere safe —
whoever holds it can sign updates for every install).

1. Bump `VERSION` (and its mirror in project.yml), commit.
2. `./build-release.sh` — signs, notarizes, staples, leaves
   `build/Passband-$VERSION.zip`.
3. Publish the archive under the passband-v tag (squelchd owns bare `v*` tags):

   ```sh
   gh release create "passband-v$(cat VERSION)" "build/Passband-$(cat VERSION).zip" \
     --title "Passband $(cat VERSION)" --notes "..."
   ```

4. `./make-appcast.sh` — stages the zip into `releases/` and regenerates
   `../passband-site/appcast.xml` with a signed entry.
5. Commit and push `passband-site/appcast.xml` — the site redeploys and every
   install sees the update on its next check.

The appcast enclosure points at `https://passband.app/download/<zip>`, which
the site 302s to the GitHub release asset, so the feed URL never depends on
where the archives are stored. `releases/` is the generator's working set:
keep it around so old entries survive regeneration; losing it only trims the
feed to the versions you re-stage.
