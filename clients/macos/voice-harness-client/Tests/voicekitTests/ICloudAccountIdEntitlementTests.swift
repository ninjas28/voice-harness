import XCTest
import Security
@testable import voicekit

/// Regression for the Start-crash (SIGABRT on 2026-09-23): on a binary
/// without the iCloud entitlement — e.g. the hand-built unsigned macOS
/// bundle — `CKContainer.default()` throws an uncatchable Objective-C
/// `CKException` ('containerIdentifier can not be nil') and the whole
/// process aborts. `ICloudAccountId.currentKey()` must probe the
/// entitlement via SecTask first and return nil without ever constructing
/// the container.
///
/// On an entitlement-less test binary (our case), the pre-fix code aborted
/// the entire test process — this test going green IS the fix. On a signed
/// binary with real entitlements the assertion would depend on live
/// CloudKit, so it skips there.
final class ICloudAccountIdEntitlementTests: XCTestCase {
    func testCurrentKeyReturnsNilOnEntitlementlessBinary() async throws {
        guard !SecEntitlementProbe.hasICloudContainerEntitlement() else {
            throw XCTSkip("binary holds the iCloud entitlement; skipping (would touch live CloudKit)")
        }
        let key = await ICloudAccountId().currentKey()
        XCTAssertNil(key, "entitlement-less binaries must fall back silently, never crash")
    }
}
