// paneflow-summarize - the on-device summariser sidecar for PaneFlow's fleet
// agent summary (issue #576).
//
// Deliberately dumb. It receives {"instructions": "...", "prompt": "..."} on
// stdin and writes {"summary": "..."} (or an error shape) to stdout. Every
// decision about what the model is told - transcript budget, fencing, what is
// asked for - lives in Rust (`src-app/src/app/agent_summary/summarize.rs`)
// where it is unit-testable without a model, a GPU, or macOS 26.
//
// This exists because FoundationModels is Swift-only: it exposes no
// Objective-C interface, so the Rust side cannot reach it through objc2.
//
// Exit code is always 0 when a JSON answer was produced, including a
// well-formed failure. A non-zero exit means the process could not answer at
// all, which the Rust caller reports as a per-pane failure.

import Foundation

#if canImport(FoundationModels)
import FoundationModels
#endif

struct Request: Decodable {
    let instructions: String
    let prompt: String
}

struct Response: Encodable {
    var summary: String?
    var error: String?
    var unavailable: Bool = false
}

/// Write the response and exit. Always one line of JSON on stdout.
func emit(_ response: Response) -> Never {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.withoutEscapingSlashes]
    if let data = try? encoder.encode(response) {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    } else {
        // Encoding our own small struct cannot realistically fail, but never
        // exit silently: the caller treats empty stdout as a failure anyway.
        FileHandle.standardOutput.write(
            "{\"error\":\"could not encode response\"}\n".data(using: .utf8)!
        )
    }
    exit(0)
}

func fail(_ message: String) -> Never {
    emit(Response(summary: nil, error: message, unavailable: false))
}

/// `unavailable` is the signal that the whole feature is off on this machine,
/// so the app stops asking rather than repeating the failure once per pane.
func unavailable(_ message: String) -> Never {
    emit(Response(summary: nil, error: message, unavailable: true))
}

// --- input -----------------------------------------------------------------

let inputData = FileHandle.standardInput.readDataToEndOfFile()
guard !inputData.isEmpty else {
    fail("no request on stdin")
}
guard let request = try? JSONDecoder().decode(Request.self, from: inputData) else {
    fail("malformed request on stdin")
}
guard !request.prompt.isEmpty else {
    fail("empty prompt")
}

// --- model -----------------------------------------------------------------

#if canImport(FoundationModels)
if #available(macOS 26, *) {
    let model = SystemLanguageModel.default

    switch model.availability {
    case .available:
        break
    case .unavailable(let reason):
        switch reason {
        case .deviceNotEligible:
            unavailable("This Mac does not support Apple Intelligence")
        case .appleIntelligenceNotEnabled:
            unavailable("Apple Intelligence is turned off in System Settings")
        case .modelNotReady:
            unavailable("The on-device model is still downloading")
        @unknown default:
            unavailable("Apple Intelligence is unavailable")
        }
    @unknown default:
        unavailable("Apple Intelligence is unavailable")
    }

    // One session per invocation: these are one-shot summaries with no
    // conversation to carry, and a fresh session keeps one pane's transcript
    // from ever entering another pane's context.
    let session = LanguageModelSession(model: model, instructions: request.instructions)

    // Low temperature: this is extraction, not writing. The token ceiling is
    // the one-sentence budget the instructions ask for, with headroom.
    let options = GenerationOptions(temperature: 0.2, maximumResponseTokens: 80)

    let semaphore = DispatchSemaphore(value: 0)
    var result: Result<String, Error>?

    Task {
        do {
            let answer = try await session.respond(to: request.prompt, options: options)
            result = .success(answer.content)
        } catch {
            result = .failure(error)
        }
        semaphore.signal()
    }
    semaphore.wait()

    switch result {
    case .success(let text):
        emit(Response(summary: text, error: nil, unavailable: false))
    case .failure(let error):
        // A guardrail refusal is a per-call outcome, not a broken feature:
        // report it as an ordinary failure so other panes still run.
        fail("model error: \(error.localizedDescription)")
    case .none:
        fail("model returned no result")
    }
} else {
    unavailable("Summaries need macOS 26 or later")
}
#else
unavailable("This build has no Foundation Models support")
#endif
