// vphone-android: reading AppleDB's catalogue (98 MB of JSON, about 5 million nodes) with Foundation's
// JSONDecoder, which scans it once into a flat map and builds only the fields decoded here.
// JSONSerialization makes an object per node and dies in bionic's allocator ("Scudo ERROR: internal
// map failure", seen on Android 17) before the parse ends. The result has the shape
// VPhoneFirmwareIndex.releases reads.
import Foundation

enum VPhoneAndroidCatalogue {
    private struct Link: Decodable {
        var active: Bool?
        var preferred: Bool?
        var url: String?
    }

    private struct Source: Decodable {
        var type: String?
        var deviceMap: [String]?
        var links: [Link]?
    }

    private struct Entry: Decodable {
        var osStr: String?
        var beta: Bool?
        var rc: Bool?
        var version: String?
        var build: String?
        var uniqueBuild: String?
        var deviceMap: [String]?
        var sources: [Source]?

        private enum Keys: String, CodingKey {
            case osStr, beta, rc, version, build, uniqueBuild, deviceMap, sources
        }

        // AppleDB's fields are not uniformly typed across 19,000 entries: a field of another
        // type reads as absent, as the `as? T` casts it replaces did.
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            osStr = (try? c.decodeIfPresent(String.self, forKey: .osStr)) ?? nil
            beta = (try? c.decodeIfPresent(Bool.self, forKey: .beta)) ?? nil
            rc = (try? c.decodeIfPresent(Bool.self, forKey: .rc)) ?? nil
            version = (try? c.decodeIfPresent(String.self, forKey: .version)) ?? nil
            build = (try? c.decodeIfPresent(String.self, forKey: .build)) ?? nil
            uniqueBuild = (try? c.decodeIfPresent(String.self, forKey: .uniqueBuild)) ?? nil
            deviceMap = (try? c.decodeIfPresent([String].self, forKey: .deviceMap)) ?? nil
            sources = (try? c.decodeIfPresent([Source].self, forKey: .sources)) ?? nil
        }
    }

    private struct Root: Decodable { var ios: [Entry] }

    static func entries(from data: Data) throws -> [[String: Any]] {
        try JSONDecoder().decode(Root.self, from: data).ios.map { entry in
            var out: [String: Any] = [:]
            out["osStr"] = entry.osStr
            out["beta"] = entry.beta
            out["rc"] = entry.rc
            out["version"] = entry.version
            out["build"] = entry.build
            out["uniqueBuild"] = entry.uniqueBuild
            out["deviceMap"] = entry.deviceMap
            out["sources"] = entry.sources?.map { source in
                [
                    "type": source.type as Any,
                    "deviceMap": source.deviceMap as Any,
                    "links": (source.links ?? []).map { link -> [String: Any] in
                        ["active": link.active as Any, "preferred": link.preferred as Any, "url": link.url as Any]
                    } as Any,
                ] as [String: Any]
            }
            return out
        }
    }
}
