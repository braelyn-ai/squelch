// Motion tokens. Two surfaces doing the same thing have to move at the same
// speed, so the durations live here rather than at the call site.

import SwiftUI

enum Motion {
    /// A list scrolling to keep the selection in view. Short enough to read as
    /// the rows sliding under the cursor rather than as an animation.
    static let scrollFollow = Animation.easeOut(duration: 0.12)
}
