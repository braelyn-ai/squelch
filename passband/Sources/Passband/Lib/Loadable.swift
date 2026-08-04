// The load state every fetch-on-appear view was hand-rolling: a value, an error
// string and an in-flight flag that only ever moved together, plus the same
// eight-line load(). A STRUCT rather than an enum because a reload has to keep
// the value it already holds — a `.loading` case that discards it blanks a list
// that is still on screen. Presentation stays in the views: this owns the state
// machine and the fetch wrapper, and nothing about how either one renders.

import SwiftUI

struct Loadable<Value> {
    /// The last value that landed; survives both a reload and a later failure.
    var value: Value?
    /// Human text from `errText`; cleared by the next success, never by a retry.
    var error: String?
    /// True only while a fetch is in flight.
    var isLoading: Bool

    /// Nothing fetched and nothing in flight — also how a view DROPS a value it
    /// must not keep holding.
    static var idle: Loadable { Loadable(value: nil, error: nil, isLoading: false) }

    /// The starting state for a view that fetches on mount, so its first frame
    /// draws the loading branch instead of the empty one.
    static var loading: Loadable { Loadable(value: nil, error: nil, isLoading: true) }
}

extension Binding {
    /// The load() the list views hand-rolled: raise the in-flight flag, await the
    /// fetch, then land either the value or `errText(_, failure)`. Written
    /// through a Binding so the write happens at each step rather than under one
    /// exclusive access held across the await.
    @MainActor
    func load<T>(_ failure: String, _ fetch: () async throws -> T) async
    where Value == Loadable<T> {
        wrappedValue.isLoading = true
        do {
            let fetched = try await fetch()
            wrappedValue.value = fetched
            wrappedValue.error = nil
        } catch {
            wrappedValue.error = errText(error, failure)
        }
        wrappedValue.isLoading = false
    }
}
