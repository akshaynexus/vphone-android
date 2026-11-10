// vphone-android overlay, over vphone-linux's file of the same name: Apple Encrypted Archive decryption
// without macOS's aea tool, with the segments spread over the phone's cores.
//
// Profile 1 (symmetric key, unsigned), the profile of every IPSW `.dmg.aea`. Layout and key
// schedule follow the reverse-engineered format (blacktop/ipsw pkg/aea):
//
//   "AEA1" | u32 profile (low 24 bits) + scrypt strength | u32 auth data size | auth data
//   32-byte main salt
//   root header: HMAC(32) | encrypted root header (48) | first cluster's HMAC(32)
//   clusters: encrypted segment headers | next cluster's HMAC | segment HMACs | segment data...
//   padding
//
// Keys come from HKDF-SHA256: the main key from the archive key ("AEA_AMK" + profile word, salted
// with the main salt), and from it the root header key ("AEA_RHEK"), each cluster's key
// ("AEA_CK" + index), that cluster's header key ("AEA_CHEK") and each segment's key ("AEA_SK" +
// index). A header key is 80 bytes: HMAC key, AES-256 key, CTR IV. Every HMAC is
// HMAC-SHA256(salt | data | u64 salt length). Each segment is AES-CTR encrypted, then optionally
// compressed (the root header says how) and checksummed.

import Crypto
import _CryptoExtras
import Foundation
import VLCompression

public enum VPhoneAEADecrypt {
    public enum Error: Swift.Error, CustomStringConvertible {
        case notAEA
        case unsupportedProfile(UInt32)
        case badKey
        case truncated
        case authentication(String)
        case unsupportedCompression(UInt8)
        case unsupportedChecksum(UInt8)
        case decompression(Int, Int)
        case checksum(Int, Int)
        case size(UInt64, UInt64)

        public var description: String {
            switch self {
            case .notAEA: "not an AEA1 archive"
            case let .unsupportedProfile(p): "AEA profile \(p) is not supported (only 1, symmetric key)"
            case .badKey: "the key is not base64:<32 bytes>"
            case .truncated: "the archive is truncated"
            case let .authentication(what): "\(what) failed authentication (wrong key or damaged archive)"
            case let .unsupportedCompression(c): "segment compression '\(Character(UnicodeScalar(c)))' is not supported"
            case let .unsupportedChecksum(c): "segment checksum type \(c) is not supported"
            case let .decompression(c, s): "cluster \(c) segment \(s) did not decompress"
            case let .checksum(c, s): "cluster \(c) segment \(s) checksum mismatch"
            case let .size(got, want): "decrypted \(got) bytes, the archive says \(want)"
            }
        }
    }

    private struct HeaderKey {
        let mac: SymmetricKey
        let key: SymmetricKey
        let iv: Data

        init(_ bytes: Data) {
            mac = SymmetricKey(data: bytes.prefix(32))
            key = SymmetricKey(data: bytes.dropFirst(32).prefix(32))
            iv = Data(bytes.dropFirst(64).prefix(16))
        }

        func decrypt(_ data: Data) throws -> Data {
            try AES._CTR.decrypt(data, using: key, nonce: AES._CTR.Nonce(nonceBytes: iv))
        }

        func verify(_ data: Data, salt: Data, expected: Data, what: @autoclosure () -> String) throws {
            var hmac = HMAC<SHA256>(key: mac)
            hmac.update(data: salt)
            hmac.update(data: data)
            var length = UInt64(salt.count).littleEndian
            hmac.update(data: Data(bytes: &length, count: 8))
            guard Data(hmac.finalize()) == expected else { throw Error.authentication(what()) }
        }
    }

    private static func derive(_ key: SymmetricKey, salt: Data = Data(), info: Data, count: Int) -> Data {
        HKDF<SHA256>.deriveKey(inputKeyMaterial: key, salt: salt, info: info, outputByteCount: count)
            .withUnsafeBytes { Data($0) }
    }

    private static func info(_ label: String, _ value: UInt32? = nil) -> Data {
        var data = Data(label.utf8)
        if var value = value?.littleEndian { data.append(Data(bytes: &value, count: 4)) }
        return data
    }

    /// The key as `aea decrypt -key-value` takes it: `base64:<key>` (or `hex:<key>`).
    public static func parseKey(_ value: String) throws -> SymmetricKey {
        let bytes: Data?
        if value.hasPrefix("base64:") {
            bytes = Data(base64Encoded: String(value.dropFirst(7)))
        } else if value.hasPrefix("hex:") {
            let hex = Array(value.dropFirst(4).utf8)
            var out = Data()
            var ok = hex.count % 2 == 0
            var i = 0
            while ok, i < hex.count {
                if let byte = UInt8(String(decoding: hex[i ..< i + 2], as: UTF8.self), radix: 16) { out.append(byte) } else { ok = false }
                i += 2
            }
            bytes = ok ? out : nil
        } else {
            bytes = Data(base64Encoded: value)
        }
        guard let bytes, bytes.count == 32 else { throw Error.badKey }
        return SymmetricKey(data: bytes)
    }

    /// How many threads decrypt segments: every core up to eight. One reader feeds them, and
    /// past eight it is the limit (measured with the same design in the app's Rust decrypter).
    static var threadCount: Int {
        if let forced = ProcessInfo.processInfo.environment["VPHONE_AEA_THREADS"].flatMap(Int.init), forced > 0 {
            return forced
        }
        return max(1, min(ProcessInfo.processInfo.activeProcessorCount, 8))
    }

    /// One segment as it lies in the archive, with what its cluster's header said of it.
    private struct Segment {
        let cluster: Int
        let number: Int
        let plainSize: Int
        let encrypted: Data
        let mac: Data
        let checksum: Data
    }

    /// The first error any thread met; the others stop when they see one.
    private final class Failure: @unchecked Sendable {
        private let lock = NSLock()
        private var error: Swift.Error?

        var isSet: Bool {
            lock.lock(); defer { lock.unlock() }
            return error != nil
        }

        func set(_ new: Swift.Error) {
            lock.lock(); defer { lock.unlock() }
            if error == nil { error = new }
        }

        func rethrow() throws {
            lock.lock(); defer { lock.unlock() }
            if let error { throw error }
        }
    }

    /// Decrypt `input` into `output` (created or truncated).
    ///
    /// The archive is read in order by this thread. Its segments are independent, each with its
    /// own key, authentication code and checksum, so they are authenticated, decrypted,
    /// decompressed, checked and written on `threadCount` threads, a batch at a time. Every check
    /// the one-thread version made is made here on every segment.
    public static func decrypt(input: URL, output: URL, key archiveKey: SymmetricKey) throws {
        let reader = try FileHandle(forReadingFrom: input)
        defer { try? reader.close() }
        func read(_ count: Int) throws -> Data {
            guard count > 0 else { return Data() }
            guard let data = try reader.read(upToCount: count), data.count == count else { throw Error.truncated }
            return data
        }
        func u32(_ data: Data, _ offset: Int) -> UInt32 {
            data.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: offset, as: UInt32.self)) }
        }
        func u64(_ data: Data, _ offset: Int) -> UInt64 {
            data.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: offset, as: UInt64.self)) }
        }

        let header = try read(12)
        guard header.prefix(4) == Data("AEA1".utf8) else { throw Error.notAEA }
        let profileWord = u32(header, 4)
        guard profileWord & 0xFFFFFF == 1 else { throw Error.unsupportedProfile(profileWord & 0xFFFFFF) }
        let authData = try read(Int(u32(header, 8)))
        let mainSalt = try read(32)
        let mainKey = SymmetricKey(data: derive(archiveKey, salt: mainSalt, info: info("AEA_AMK", profileWord), count: 32))

        let rootMAC = try read(32)
        let encryptedRoot = try read(48)
        var clusterMAC = try read(32)
        let rootKey = HeaderKey(derive(mainKey, info: info("AEA_RHEK"), count: 80))
        try rootKey.verify(encryptedRoot, salt: clusterMAC + authData, expected: rootMAC, what: "the root header")
        let root = try rootKey.decrypt(encryptedRoot)
        let fileSize = u64(root, 0)
        let segmentSize = Int(u32(root, 16))
        let segmentsPerCluster = Int(u32(root, 20))
        let compression = root[root.startIndex + 24]
        let checksumType = root[root.startIndex + 25]
        let checksumSize: Int
        switch checksumType {
        case 0: checksumSize = 0
        case 1: checksumSize = 8
        case 2: checksumSize = 32
        default: throw Error.unsupportedChecksum(checksumType)
        }
        let algorithm: compression_algorithm?
        switch compression {
        case UInt8(ascii: "-"): algorithm = nil
        case UInt8(ascii: "e"): algorithm = COMPRESSION_LZFSE
        case UInt8(ascii: "z"): algorithm = COMPRESSION_ZLIB
        case UInt8(ascii: "x"): algorithm = COMPRESSION_LZMA
        case UInt8(ascii: "4"): algorithm = COMPRESSION_LZ4
        default: throw Error.unsupportedCompression(compression)
        }

        let descriptor = open(output.path, O_WRONLY | O_CREAT | O_TRUNC, 0o644)
        guard descriptor >= 0 else {
            throw CocoaError(.fileWriteUnknown, userInfo: [NSFilePathErrorKey: output.path])
        }
        defer { close(descriptor) }
        guard ftruncate(descriptor, off_t(fileSize)) == 0 else {
            throw CocoaError(.fileWriteOutOfSpace, userInfo: [NSFilePathErrorKey: output.path])
        }

        let threads = threadCount
        let failure = Failure()
        // What one thread does with one segment. Writes go to disjoint ranges of the file.
        func process(_ segment: Segment, clusterKey: SymmetricKey) {
            guard !failure.isSet else { return }
            do {
                let segmentKey = HeaderKey(derive(clusterKey, info: info("AEA_SK", UInt32(segment.number)), count: 80))
                try segmentKey.verify(
                    segment.encrypted, salt: Data(), expected: segment.mac,
                    what: "cluster \(segment.cluster) segment \(segment.number)",
                )
                let stored = try segmentKey.decrypt(segment.encrypted)
                let plain: Data
                if segment.plainSize == stored.count || algorithm == nil {
                    plain = stored
                } else {
                    // Straight into the buffer that is checked and written: no second copy.
                    var decompressed = Data(count: segment.plainSize)
                    let n = stored.withUnsafeBytes { src in
                        decompressed.withUnsafeMutableBytes { dst in
                            compression_decode_buffer(
                                dst.bindMemory(to: UInt8.self).baseAddress!, segment.plainSize,
                                src.bindMemory(to: UInt8.self).baseAddress!, stored.count, nil, algorithm!,
                            )
                        }
                    }
                    guard n == segment.plainSize else { throw Error.decompression(segment.cluster, segment.number) }
                    plain = decompressed
                }
                guard plain.count == segment.plainSize else { throw Error.decompression(segment.cluster, segment.number) }
                switch checksumType {
                case 2:
                    guard Data(SHA256.hash(data: plain)) == segment.checksum else {
                        throw Error.checksum(segment.cluster, segment.number)
                    }
                case 1:
                    // MurmurHash64A, seed 0xE2236FDC26A5F6D2.
                    guard murmur64(plain) == u64(segment.checksum, 0) else {
                        throw Error.checksum(segment.cluster, segment.number)
                    }
                default:
                    break
                }
                let offset = UInt64(segment.cluster * segmentsPerCluster + segment.number) * UInt64(segmentSize)
                guard offset + UInt64(plain.count) <= fileSize else { throw Error.size(offset + UInt64(plain.count), fileSize) }
                // A segment of zeros stays a hole: the file was sized above, and reads back the same.
                try plain.withUnsafeBytes { bytes in
                    // All zeros when the first byte is zero and every byte equals the one before it.
                    let zeros = bytes[0] == 0 && memcmp(bytes.baseAddress!, bytes.baseAddress! + 1, bytes.count - 1) == 0
                    if !zeros {
                        var done = 0
                        while done < bytes.count {
                            let n = pwrite(descriptor, bytes.baseAddress! + done, bytes.count - done, off_t(offset) + off_t(done))
                            if n < 0, errno == EINTR { continue }
                            guard n > 0 else {
                                throw CocoaError(.fileWriteUnknown, userInfo: [NSFilePathErrorKey: output.path])
                            }
                            done += n
                        }
                    }
                }
            } catch {
                failure.set(error)
            }
        }

        let segmentHeaderSize = 8 + checksumSize
        let batchSize = threads * 4
        var produced: UInt64 = 0
        var cluster = 0
        while produced < fileSize {
            let clusterKey = SymmetricKey(data: derive(mainKey, info: info("AEA_CK", UInt32(cluster)), count: 32))
            let clusterHeaderKey = HeaderKey(derive(clusterKey, info: info("AEA_CHEK"), count: 80))
            let encryptedHeaders = try read(segmentHeaderSize * segmentsPerCluster)
            let nextClusterMAC = try read(32)
            let segmentMACs = try read(32 * segmentsPerCluster)
            try clusterHeaderKey.verify(
                encryptedHeaders, salt: nextClusterMAC + segmentMACs, expected: clusterMAC,
                what: "cluster \(cluster)'s headers",
            )
            let headers = try clusterHeaderKey.decrypt(encryptedHeaders)
            var batch: [Segment] = []
            batch.reserveCapacity(batchSize)
            func flush() throws {
                guard !batch.isEmpty else { return }
                let work = batch
                batch.removeAll(keepingCapacity: true)
                if threads == 1 || work.count == 1 {
                    for segment in work { process(segment, clusterKey: clusterKey) }
                } else {
                    DispatchQueue.concurrentPerform(iterations: work.count) { process(work[$0], clusterKey: clusterKey) }
                }
                try failure.rethrow()
            }
            for segment in 0 ..< segmentsPerCluster {
                let h = segment * segmentHeaderSize
                let plainSize = Int(u32(headers, h))
                let storedSize = Int(u32(headers, h + 4))
                guard plainSize > 0 else { continue }
                batch.append(Segment(
                    cluster: cluster,
                    number: segment,
                    plainSize: plainSize,
                    encrypted: try read(storedSize),
                    mac: Data(segmentMACs.dropFirst(32 * segment).prefix(32)),
                    checksum: Data(headers.dropFirst(h + 8).prefix(checksumSize)),
                ))
                produced += UInt64(plainSize)
                if batch.count == batchSize { try flush() }
            }
            try flush()
            clusterMAC = nextClusterMAC
            cluster += 1
        }
        guard produced == fileSize else { throw Error.size(produced, fileSize) }
    }

    /// MurmurHash64A (Austin Appleby), the checksum AEA's "murmur" type uses.
    static func murmur64(_ data: Data, seed: UInt64 = 0xE223_6FDC_26A5_F6D2) -> UInt64 {
        let m: UInt64 = 0xC6A4_A793_5BD1_E995
        let r: UInt64 = 47
        var h = seed ^ (UInt64(data.count) &* m)
        data.withUnsafeBytes { raw in
            let n = data.count / 8
            for i in 0 ..< n {
                var k = UInt64(littleEndian: raw.loadUnaligned(fromByteOffset: i * 8, as: UInt64.self))
                k = k &* m; k ^= k >> r; k = k &* m
                h ^= k; h = h &* m
            }
            let tail = data.count & 7
            if tail > 0 {
                var t: UInt64 = 0
                for j in (0 ..< tail).reversed() { t = (t << 8) | UInt64(raw[n * 8 + j]) }
                h ^= t; h = h &* m
            }
        }
        h ^= h >> r; h = h &* m; h ^= h >> r
        return h
    }
}
