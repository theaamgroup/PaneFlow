// Verify the exact archive signature against the public key shipped in the app.
// Usage: swift scripts/verify-update.swift Info.plist appcast.xml update.dmg
import Foundation
import CryptoKit

final class Feed: NSObject, XMLParserDelegate {
    var enclosures: [[String: String]] = []
    func parser(_ parser: XMLParser, didStartElement elementName: String,
                namespaceURI: String?, qualifiedName qName: String?,
                attributes: [String: String]) {
        if elementName == "enclosure" { enclosures.append(attributes) }
    }
}

enum VerificationError: Error { case invalid(String) }

func verify(key: Data, signature: Data, archive: Data) throws {
    let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: key)
    guard publicKey.isValidSignature(signature, for: archive) else {
        throw VerificationError.invalid("Archive signature does not match the app's public key")
    }
}

do {
    if CommandLine.arguments.dropFirst() == ["--self-test"] {
        let key = Curve25519.Signing.PrivateKey()
        let archive = Data("PaneFlow update fixture".utf8)
        let signature = try key.signature(for: archive)
        try verify(key: key.publicKey.rawRepresentation, signature: signature, archive: archive)
        do {
            try verify(key: key.publicKey.rawRepresentation, signature: signature, archive: archive + Data([0]))
            fatalError("Accepted a tampered archive")
        } catch VerificationError.invalid(_) {}
        do {
            try verify(key: Curve25519.Signing.PrivateKey().publicKey.rawRepresentation, signature: signature, archive: archive)
            fatalError("Accepted the wrong signing key")
        } catch VerificationError.invalid(_) {}
        print("Signature checks passed: valid archive accepted; tampering and wrong key rejected")
        exit(0)
    }
    guard CommandLine.arguments.count == 4 else {
        throw VerificationError.invalid("Usage: verify-update.swift Info.plist appcast.xml update.dmg")
    }
    let plist = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
    guard let info = try PropertyListSerialization.propertyList(from: plist, format: nil) as? [String: Any],
          let encodedKey = info["SUPublicEDKey"] as? String,
          let key = Data(base64Encoded: encodedKey), key.count == 32 else {
        throw VerificationError.invalid("Info.plist has no valid Ed25519 public key")
    }
    let feed = Feed()
    let parser = XMLParser(data: try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
    parser.shouldResolveExternalEntities = false
    parser.delegate = feed
    guard parser.parse(), feed.enclosures.count == 1,
          let enclosure = feed.enclosures.first,
          let signatureText = enclosure["sparkle:edSignature"],
          let signature = Data(base64Encoded: signatureText), signature.count == 64,
          let lengthText = enclosure["length"], let length = Int(lengthText), length > 0,
          let urlText = enclosure["url"], let url = URL(string: urlText), url.scheme == "https",
          url.lastPathComponent == URL(fileURLWithPath: CommandLine.arguments[3]).lastPathComponent else {
        throw VerificationError.invalid("Invalid appcast enclosure or archive filename mismatch")
    }
    let archive = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[3]), options: .mappedIfSafe)
    guard archive.count == length else { throw VerificationError.invalid("Archive length differs from appcast") }
    try verify(key: key, signature: signature, archive: archive)
    print("Verified \(url.lastPathComponent): \(length) bytes; signature matches the shipped public key")
    print("SHA256: " + SHA256.hash(data: archive).map { String(format: "%02x", $0) }.joined())
} catch {
    fputs("Update verification failed: \(error)\n", stderr)
    exit(1)
}
