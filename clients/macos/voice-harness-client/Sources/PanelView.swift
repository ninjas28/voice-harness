import SwiftUI
import voicekit

struct PanelView: View {
    @ObservedObject var runtime: AppRuntime

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Circle().fill(phaseColor).frame(width: 10, height: 10)
                Text(phaseLabel).font(.headline)
                Spacer()
                Button(runtime.running ? "Stop" : "Start") {
                    Task { await runtime.toggle() }
                }
                .buttonStyle(.borderedProminent)
                .disabled(runtime.isBusy)
            }
            Picker("Speed", selection: $runtime.playbackRate) {
                ForEach(AppSettings.ttsRateChoices, id: \.self) { rate in
                    Text(String(format: "%.1f×", rate)).tag(rate)
                }
            }
            .pickerStyle(.segmented)
            .controlSize(.small)
            if let err = runtime.errorMessage {
                Text(err).foregroundStyle(.red).font(.caption)
            }
            if let transcript = runtime.transcript, !transcript.isEmpty {
                VStack(alignment: .leading) {
                    Text("You said").font(.caption).foregroundStyle(.secondary)
                    Text(transcript).font(.body)
                }
            }
            HStack {
                Text("Assistant").font(.caption).foregroundStyle(.secondary)
                if runtime.phase == .thinking { ThinkingDots() }
            }
            Text(displayText)
                .font(.body)
                .frame(minHeight: 40, alignment: .topLeading)
                .frame(maxWidth: .infinity, alignment: .leading)
            if runtime.running {
                HStack {
                    Text("Server").font(.caption).foregroundStyle(.secondary)
                    Text(runtime.serverURL.absoluteString)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
        }
        .padding(14)
        // MenuBarExtra(.window) freezes the panel at its first-measured height;
        // fixedSize(vertical) lets the window grow/shrink with the content so
        // long transcripts are never truncated.
        .fixedSize(horizontal: false, vertical: true)
    }

    private var displayText: String {
        if runtime.responseText.isEmpty && runtime.phase == .thinking { return "…" }
        return runtime.responseText.isEmpty ? " " : runtime.responseText
    }

    private var phaseColor: Color {
        switch runtime.phase {
        case .idle: .gray
        case .listening: .green
        case .speech: .orange
        case .thinking: .yellow
        case .speaking: .blue
        }
    }

    private var phaseLabel: String {
        switch runtime.phase {
        case .idle: "Idle"
        case .listening: "Listening…"
        case .speech: "Hearing you…"
        case .thinking: "Thinking…"
        case .speaking: "Speaking"
        }
    }
}

struct ThinkingDots: View {
    @State private var active = 0
    var body: some View {
        HStack(spacing: 3) {
            ForEach(0..<3) { i in
                Circle().frame(width: 5, height: 5)
                    .opacity(active == i ? 1 : 0.3)
            }
        }
        .onAppear {
            withAnimation(.easeInOut(duration: 0.5).repeatForever()) {
                active = (active + 1) % 3
            }
        }
    }
}
