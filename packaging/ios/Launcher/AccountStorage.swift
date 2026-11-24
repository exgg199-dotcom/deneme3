import Foundation
import Security

struct AccountSecrets: Codable, Equatable {
    let refresh: String
    let token: String
    let expires: Date
}

/// Injectable for tests: fixtures never access the user's Keychain.
protocol AccountVault {
    func read(_ account: String) throws -> AccountSecrets?
    func write(_ secrets: AccountSecrets, for account: String) throws
    func remove(_ account: String) throws
}

struct KeychainAccountVault: AccountVault {
    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: "dev.horizon.launcher.microsoft",
         kSecAttrAccount as String: account]
    }
    func read(_ account: String) throws -> AccountSecrets? {
        var search = query(account)
        search[kSecReturnData as String] = true
        search[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        let status = SecItemCopyMatching(search as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = item as? Data else { throw storageError() }
        guard let secrets = try? JSONDecoder().decode(AccountSecrets.self, from: data) else {
            throw AccountError("Saved sign-in data could not be read. Sign in again.", reauthenticate: true)
        }
        return secrets
    }
    func write(_ secrets: AccountSecrets, for account: String) throws {
        let data = try JSONEncoder().encode(secrets)
        // Update atomically. Never delete a working credential before its replacement is saved.
        let values: [String: Any] = [kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly]
        var status = SecItemUpdate(query(account) as CFDictionary, values as CFDictionary)
        if status == errSecItemNotFound {
            var item = query(account)
            values.forEach { item[$0.key] = $0.value }
            status = SecItemAdd(item as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw storageError() }
    }
    func remove(_ account: String) throws {
        let status = SecItemDelete(query(account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw storageError() }
    }
    private func storageError() -> AccountError {
        AccountError("Sign-in could not be saved securely. Unlock this device and try again.")
    }
}

struct AccountError: LocalizedError {
    let errorDescription: String?
    let reauthenticate: Bool
    let temporarilyUnavailable: Bool
    init(_ message: String, reauthenticate: Bool = false, temporarilyUnavailable: Bool = false) {
        errorDescription = message
        self.reauthenticate = reauthenticate
        self.temporarilyUnavailable = temporarilyUnavailable
    }

    static func temporaryHTTP(_ status: Int?) -> Bool {
        guard let status else { return false }
        return status == 408 || status == 429 || (500...599).contains(status)
    }
}

/// HTTPS authentication requests. Responses never enter logs or error strings.
struct AccountNet {
    static let retryAfterHeader = "retryAfterHeader"

    static func retryDelay(_ header: String?, now: Date) -> TimeInterval {
        guard let header else { return 0 }
        let value = header.trimmingCharacters(in: .whitespaces)
        if !value.isEmpty, value.utf8.allSatisfy({ (48...57).contains($0) }) {
            return Double(value) ?? .infinity
        }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = formatter.timeZone
        formatter.twoDigitStartDate = calendar.date(byAdding: .year, value: -49, to: now)
        for format in ["EEE, dd MMM yyyy HH:mm:ss 'GMT'", "EEEE, dd-MMM-yy HH:mm:ss 'GMT'", "EEE MMM d HH:mm:ss yyyy"] {
            formatter.dateFormat = format
            if let date = formatter.date(from: value) {
                return max(0, date.timeIntervalSince(now))
            }
        }
        return 0
    }

    let session: URLSession
    func form(
        _ url: String, _ fields: [String: String], allowErrors: Bool = false,
        deadline: ContinuousClock.Instant? = nil
    ) async throws -> [String: Any] {
        var request = URLRequest(url: URL(string: url)!)
        request.httpMethod = "POST"
        request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        var allowed = CharacterSet.alphanumerics; allowed.insert(charactersIn: "-._~")
        request.httpBody = fields.map { "\($0.key)=\($0.value.addingPercentEncoding(withAllowedCharacters: allowed) ?? "")" }
            .joined(separator: "&").data(using: .utf8)
        return try await send(request, allowErrors: allowErrors, deadline: deadline)
    }
    func json(_ url: String, _ body: [String: Any], allowErrors: Bool = false) async throws -> [String: Any] {
        var request = URLRequest(url: URL(string: url)!)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        return try await send(request, allowErrors: allowErrors)
    }
    func get(_ url: String, token: String, allowErrors: Bool = false) async throws -> [String: Any] {
        var request = URLRequest(url: URL(string: url)!)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return try await send(request, allowErrors: allowErrors)
    }
    private func data(
        for request: URLRequest, deadline: ContinuousClock.Instant?
    ) async throws -> (Data, URLResponse) {
        guard let deadline else { return try await session.data(for: request) }
        let expired = AccountError("The code expired. Try again.")
        guard ContinuousClock.now < deadline else { throw expired }
        return try await withThrowingTaskGroup(of: (Data, URLResponse).self) { group in
            group.addTask {
                guard ContinuousClock.now < deadline else { throw expired }
                return try await session.data(for: request)
            }
            group.addTask {
                try await ContinuousClock().sleep(until: deadline)
                throw expired
            }
            defer { group.cancelAll() }
            guard let response = try await group.next() else { throw CancellationError() }
            guard ContinuousClock.now < deadline else { throw expired }
            return response
        }
    }

    private func send(
        _ input: URLRequest, allowErrors: Bool, deadline: ContinuousClock.Instant? = nil
    ) async throws -> [String: Any] {
        try Task.checkCancellation()
        var request = input; request.timeoutInterval = 30
        let result: (Data, URLResponse)
        do { result = try await data(for: request, deadline: deadline) }
        catch {
            try Task.checkCancellation()
            if let error = error as? AccountError { throw error }
            let code = (error as? URLError)?.code
            let temporary = [URLError.Code.timedOut, .networkConnectionLost, .notConnectedToInternet,
                             .cannotConnectToHost, .cannotFindHost, .dnsLookupFailed,
                             .dataNotAllowed, .internationalRoamingOff].contains { $0 == code }
            throw AccountError("Could not reach the sign-in service. Check your connection and try again.",
                               temporarilyUnavailable: temporary)
        }
        try Task.checkCancellation()
        guard let response = result.1 as? HTTPURLResponse else {
            throw AccountError("The sign-in service returned an incomplete response. Try again.")
        }
        let status = response.statusCode
        let body = (try? JSONSerialization.jsonObject(with: result.0)) as? [String: Any]
        if !(200..<300).contains(status) {
            guard allowErrors else {
                throw AccountError("The sign-in service is unavailable (\(status)). Try again.",
                                   temporarilyUnavailable: AccountError.temporaryHTTP(status))
            }
            var error = body ?? [:]
            error["status"] = status
            error[Self.retryAfterHeader] = response.value(forHTTPHeaderField: "Retry-After")
            return error
        }
        guard let body else {
            throw AccountError("The sign-in service returned an incomplete response. Try again.")
        }
        return body
    }
}
