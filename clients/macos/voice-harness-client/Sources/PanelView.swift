import SwiftUI
import voicekit

struct PanelView: View {
    @ObservedObject var runtime: AppRuntime
    @State private var showSettings = false
    @State private var settingsError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Circle().fill(phaseColor).frame(width: 10, height: 10)
                Text(phaseLabel).font(.headline)
                Spacer()
                Button {
                    showSettings.toggle()
                    if !showSettings { settingsError = nil }
                } label: {
                    Image(systemName: showSettings ? "gearshape.fill" : "gearshape")
                }
                .buttonStyle(.bordered)
                .help("Server settings")
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
            if showSettings {
                settingsSection
            }
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
                Text("Assistant")
                    .font(.caption)
                    .foregroundStyle(assistantColor)
                if runtime.activity != .idle { ThinkingDots() }
                if !assistantStatus.isEmpty {
                    Text(assistantStatus)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Text(displayText)
                .font(.body)
                .frame(minHeight: 40, alignment: .topLeading)
                .frame(maxWidth: .infinity, alignment: .leading)
            if runtime.running {
                HStack {
                    Text("Server").font(.caption).foregroundStyle(.secondary)
                    Text(AppSettings.serverURL.absoluteString)
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

    /// Editable server endpoint. Saving validates + persists via the runtime
    /// and stops a running session so the next start reconnects.
    private var settingsSection: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Server URL").font(.caption).foregroundStyle(.secondary)
            TextField("ws://host:8090/v1/realtime", text: $runtime.serverURLString)
                .textFieldStyle(.roundedBorder)
                .onSubmit(saveURL)
            Text("API key").font(.caption).foregroundStyle(.secondary)
            SecureField("empty = no auth", text: $runtime.apiKeyString)
                .textFieldStyle(.roundedBorder)
                .onSubmit(saveURL)
            // Personal context: on = request calendar/contacts/photos access
            // (the one deliberate TCC prompt) and announce the authorized
            // tools to the server; off = announce an empty list, which clears
            // the server-side catalog.
            Toggle("Personal context (calendar, contacts, photos)",
                   isOn: Binding(
                       get: { runtime.personalContextEnabled },
                       set: { runtime.setPersonalContextEnabled($0) }))
                .font(.caption)
            // Raw stores (v1.5): on = probe Messages/Mail/Notes access (the
            // deliberate FDA + Automation TCC moment) and re-announce; off =
            // re-announce the remaining v1 catalog, clearing the raw tools.
            Toggle("Raw stores (messages, mail, notes)",
                   isOn: Binding(
                       get: { runtime.rawStoresEnabled },
                       set: { runtime.setRawStoresEnabled($0) }))
                .font(.caption)
            Text("Reads Messages, Mail, and Notes data on this Mac. Requires Full Disk Access and Automation permission.")
                .font(.caption2)
                .foregroundStyle(.secondary)
            if let settingsError {
                Text(settingsError).font(.caption).foregroundStyle(.red)
            }
            HStack {
                Button("Save", action: saveURL)
                    .buttonStyle(.borderedProminent)
                    .disabled(runtime.serverURLString == AppSettings.serverURL.absoluteString
                              && runtime.apiKeyString == AppSettings.apiKey)
                Button("Reset") {
                    runtime.serverURLString = AppSettings.defaultServerURLString
                    settingsError = nil
                }
                Spacer()
                if runtime.running {
                    Text("Saving stops the session")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private func saveURL() {
        if let error = runtime.commitServerURL(runtime.serverURLString, apiKey: runtime.apiKeyString) {
            settingsError = error
        } else {
            settingsError = nil
            showSettings = false
        }
    }

    /// Assistant-row state. Dots run whenever the model is under `thinking`;
    /// `calling tools…` surfaces the tool round that used to be invisible.
    private var assistantStatus: String {
        switch runtime.activity {
        case .callingTools: "calling tools…"
        case .thinking, .idle: ""
        }
    }

    private var assistantColor: Color {
        switch runtime.activity {
        case .idle: .gray
        case .thinking: .yellow
        case .callingTools: .orange
        }
    }

    /// Empty while plain thinking; `… (calling tools)` while the model is
    /// inside a tool round so the text itself distinguishes the sub-state.
    private var displayText: String {
        if runtime.responseText.isEmpty && runtime.phase == .thinking {
            return runtime.activity == .callingTools ? "… (calling tools)" : "…"
        }
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
