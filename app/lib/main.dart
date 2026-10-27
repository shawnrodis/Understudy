/// Understudy tray app (Flutter).
///
/// This UI is deliberately NOT a remote-desktop viewer and NOT the control
/// path. The control API (CONTROL_API.md) is the primary interface, used
/// headlessly by the AI operator. This app exists to:
///   * launch and watch over the engine process,
///   * show connection status, monitors, and the connected indicator,
///   * show the action log,
///   * offer the kill switch (Quit stops the engine and closes everything),
///   * walk first-launch Tailscale setup (auto-install + one-time login).
///
/// Closing the window tucks the app to the tray; Quit (tray menu or the
/// Status screen's button) stops the engine and exits for real.
library;

import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:tray_manager/tray_manager.dart';
import 'package:window_manager/window_manager.dart';

import 'api_client.dart';
import 'update.dart';

/// Loopback port the single-instance guard listens on. Unrelated to the
/// engine's control port (18769): this one exists only so a second app
/// launch can find the first and hand off to it.
const _singleInstancePort = 18771;

void main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await windowManager.ensureInitialized();
  await _enforceSingleInstance();
  // X hides the window to the tray; it never quits. Quitting happens
  // only via the tray menu's Quit entry or the in-app Quit button.
  await windowManager.setPreventClose(true);
  runApp(const UnderstudyApp());
}

/// Exactly one window, one tray icon, one taskbar button. If another
/// Understudy window is already running, ask it to show itself and exit
/// without creating a duplicate window, engine, or taskbar icon. A
/// crashed previous instance leaves the port free, so a fresh launch
/// still works.
Future<void> _enforceSingleInstance() async {
  try {
    final probe = await Socket.connect(
      InternetAddress.loopbackIPv4,
      _singleInstancePort,
      timeout: const Duration(milliseconds: 300),
    );
    // Any connection wakes the existing instance; no data needed.
    probe.destroy();
    exit(0);
  } catch (_) {
    // No existing instance listening; fall through and become the one.
  }
  try {
    final server = await ServerSocket.bind(
      InternetAddress.loopbackIPv4,
      _singleInstancePort,
    );
    server.listen((client) async {
      client.destroy();
      await windowManager.show();
      await windowManager.focus();
    });
  } catch (_) {
    // If the guard port cannot be bound, run anyway rather than brick
    // the launch; the window still opens, just without the guard.
  }
}

class UnderstudyApp extends StatelessWidget {
  const UnderstudyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Understudy',
      debugShowCheckedModeBanner: false,
      theme: ThemeData.dark().copyWith(
        colorScheme: ColorScheme.fromSeed(
          seedColor: const Color(0xFF1E1B4B),
          brightness: Brightness.dark,
        ).copyWith(secondary: const Color(0xFFF59E0B)),
        // Material 3 is the default in current Flutter; the old
        // useMaterial3 flag is deprecated and intentionally omitted.
      ),
      home: const HomeScreen(),
    );
  }
}

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> with WindowListener {
  int _tab = 0;
  Process? _engine;
  ApiClient? _api;
  Map<String, dynamic>? _status;
  List<dynamic> _monitors = [];
  List<dynamic> _log = [];
  Map<String, dynamic>? _ts;
  String? _notice;
  bool _engineStarting = false;
  // Engine lifecycle: 'starting' while a spawn/probe is in flight,
  // 'running' once a status probe has answered, 'failed' otherwise.
  // Tabs are always reachable now; this state drives a card on the
  // Status tab instead of replacing every tab with a dead screen.
  String _engineState = 'starting';
  String? _engineFailure;
  DateTime? _lastSpawnAttempt;
  bool _quitting = false;
  UpdateInfo? _update;
  bool _updateChecking = false;
  String? _updateMessage;
  bool _tokenRevealed = false;
  Timer? _poller;

  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    _initTray();
    _ensureEngine();
    _autoUpdateCheck();
    _poller = Timer.periodic(const Duration(seconds: 3), (_) => _tick());
  }

  Future<void> _initTray() async {
    // The icon and the menu are set independently on purpose: if the
    // icon fails, the menu — including Quit — must still exist. One
    // try block around both is how the Quit entry goes missing.
    try {
      final exeDir = File(Platform.resolvedExecutable).parent;
      final ico = File('${exeDir.path}/icon.ico');
      if (await ico.exists()) {
        await trayManager.setIcon(ico.path);
      }
    } catch (_) {
      // Tray icon is a convenience; the app must work without it.
    }
    try {
      await trayManager.setContextMenu(
        Menu(items: [
          MenuItem(
            label: 'Show Understudy',
            onClick: (_) => windowManager.show(),
          ),
          MenuItem(
            label: 'Quit (kill switch — stops the engine)',
            onClick: (_) => _quit(),
          ),
        ]),
      );
    } catch (_) {
      // Without a menu there is no tray Quit; the in-app Quit button
      // on the Status tab remains the kill switch.
    }
  }

  @override
  void onWindowClose() async {
    // X tucks to tray; Quit is the real exit (the kill switch).
    await windowManager.hide();
  }

  /// Make sure an engine is running and connected, spawning one from
  /// this app's own install folder when needed. Safe to call repeatedly:
  /// it probes for a live engine first and never spawns a second one
  /// while one answers. [manual] bypasses the auto-respawn cooldown
  /// (the "Start engine" button); automatic retries stay throttled so
  /// an instantly-crashing engine cannot hot-loop spawns.
  Future<void> _ensureEngine({bool manual = false}) async {
    if (_engineStarting) return;
    if (!manual &&
        _lastSpawnAttempt != null &&
        DateTime.now().difference(_lastSpawnAttempt!) <
            const Duration(seconds: 15)) {
      return;
    }
    _engineStarting = true;
    _lastSpawnAttempt = DateTime.now();
    if (mounted) {
      setState(() {
        _engineState = 'starting';
        _engineFailure = null;
      });
    }
    try {
      // 1. Is an engine already answering? (User-launched, or a previous
      // spawn — see _probeLive for why a bare connection.json is not
      // trusted on its own.)
      if (await _probeLive()) return;
      // 2. A connection.json exists but nothing answers yet: the engine
      // may be mid-startup. Give it a few seconds before spawning.
      if (await readEngineInfo() != null) {
        for (var i = 0; i < 6; i++) {
          await Future<void>.delayed(const Duration(milliseconds: 500));
          if (await _probeLive()) return;
        }
      }
      // 3. Spawn the engine from this app's own install folder. No setup
      // prerequisite, no manual step.
      final exeDir = File(Platform.resolvedExecutable).parent;
      final engineExe = File('${exeDir.path}/understudy-engine.exe');
      if (!await engineExe.exists()) {
        throw Exception(
            'Engine binary not found next to the app (${engineExe.path}). '
            'Reinstall Understudy.');
      }
      final dir = await dataDir();
      final proc = await Process.start(
        engineExe.path,
        ['--data-dir', dir.path],
        mode: ProcessStartMode.detached,
      );
      _engine = proc;
      // If the engine dies, say so instead of spinning forever. The
      // _quitting flag keeps an intentional Quit from flagging a failure.
      unawaited(proc.exitCode.then((code) {
        if (_quitting || !mounted || _engine != proc) return;
        setState(() {
          _api = null;
          _engineState = 'failed';
          _engineFailure =
              'Engine exited (code $code). Use Start engine to retry; '
              'if it keeps exiting, another copy may be holding the '
              'control port.';
        });
      }));
      // 4. Wait for readiness, confirmed with a live probe — not merely
      // the appearance of connection.json.
      for (var i = 0; i < 30; i++) {
        await Future<void>.delayed(const Duration(milliseconds: 500));
        if (await _probeLive()) return;
      }
      if (mounted) {
        setState(() {
          _engineState = 'failed';
          _engineFailure =
              'Engine did not become ready within 15s. Use Start engine '
              'to retry.';
        });
      }
    } catch (e) {
      if (mounted) {
        setState(() {
          _engineState = 'failed';
          _engineFailure = 'Engine start failed: $e';
        });
      }
    } finally {
      _engineStarting = false;
    }
  }

  /// True when connection.json exists AND the engine answers /status.
  /// A bare connection.json proves nothing on its own: the file survives
  /// uninstalls and dead engines (the data dir is intentionally kept),
  /// so trusting it without a live probe is what wedged fresh installs
  /// on a dead "Waiting for the engine…" screen forever.
  Future<bool> _probeLive() async {
    try {
      final info = await readEngineInfo();
      if (info == null) return false;
      final api = ApiClient(info);
      await api.status();
      if (!mounted) return false;
      _api = api;
      setState(() {
        _engineState = 'running';
        _engineFailure = null;
      });
      await _refresh();
      return _api != null;
    } catch (_) {
      return false;
    }
  }

  /// Periodic maintenance: refresh when connected, otherwise make sure
  /// an engine is coming up (throttled automatic respawn).
  Future<void> _tick() async {
    if (_api != null) {
      await _refresh();
    } else if (_engineState != 'starting') {
      await _ensureEngine();
    }
  }

  /// Automatic launch check: silent unless a newer release exists.
  /// Independent of the engine — it talks to GitHub directly.
  Future<void> _autoUpdateCheck() async {
    final info = await checkForUpdate();
    if (!mounted) return;
    if (info != null && info.available) {
      setState(() => _update = info);
    }
  }

  Future<void> _manualUpdateCheck() async {
    setState(() {
      _updateChecking = true;
      _updateMessage = null;
    });
    final info = await checkForUpdate(manual: true);
    if (!mounted) return;
    setState(() {
      _updateChecking = false;
      if (info == null) {
        _updateMessage = 'Update check failed.';
      } else if (info.error != null) {
        _updateMessage = 'Update check failed: ${info.error}';
      } else if (info.available) {
        _update = info;
        _updateMessage = null;
      } else {
        _updateMessage = 'You are up to date (v$kAppVersion).';
      }
    });
  }

  Future<void> _downloadAndInstall() async {
    final info = _update;
    if (info == null || info.downloadUrl.isEmpty) return;
    setState(() => _updateMessage = 'Downloading update…');
    try {
      final path = await downloadInstaller(info.downloadUrl, info.version);
      await launchInstallerAfterQuit(path, pid);
      // The installer launches once this process exits; quitting now
      // is what lets it replace the running files.
      await _quit();
    } catch (e) {
      if (mounted) {
        setState(() => _updateMessage = 'Download failed: $e');
      }
    }
  }

  Future<void> _refresh() async {
    final api = _api;
    if (api == null) {
      // The manual refresh button doubles as a recovery button.
      await _ensureEngine();
      return;
    }
    try {
      final status = await api.status();
      final mons = await api.monitors();
      final logRes = await api.log(limit: 200);
      final ts = await api.tailscaleStatus();
      if (!mounted) return;
      setState(() {
        _status = status;
        _monitors = (mons['monitors'] as List<dynamic>?) ?? [];
        _log = (logRes['entries'] as List<dynamic>?) ?? [];
        _ts = ts;
        _engineState = 'running';
        _engineFailure = null;
      });
    } on SocketException {
      // Connection refused: the engine is gone (or never started). Do
      // NOT surface the raw exception: Dart reports the client's own
      // ephemeral local port in SocketException.port, not the engine's
      // port — that is what made a dead engine look like a port
      // mismatch. Drop the dead client so the watchdog respawns
      // instead of hammering the same dead endpoint forever.
      if (!mounted) return;
      setState(() {
        _api = null;
        _engineState = 'failed';
        _engineFailure =
            'Engine unreachable at ${api.info.base} — the engine is not '
            'running. It will be restarted automatically, or use Start '
            'engine.';
      });
    } catch (e) {
      if (mounted) {
        setState(() => _engineFailure = 'Engine request failed: $e');
      }
    }
  }

  Future<void> _rotateToken() async {
    final api = _api;
    if (api == null) return;
    try {
      await api.rotateToken();
      // The old token is dead now; re-read the fresh one from disk so
      // this UI (and its reveal field) keeps working.
      final info = await readEngineInfo();
      if (info != null) _api = ApiClient(info);
      setState(() => _notice =
          'Pairing token rotated — the old token is now invalid.');
      await _refresh();
    } catch (e) {
      setState(() => _notice = 'Token rotation failed: $e');
    }
  }

  Future<void> _quit() async {
    _quitting = true;
    try {
      await _api?.shutdown();
    } catch (_) {}
    try {
      _engine?.kill();
    } catch (_) {}
    await windowManager.setPreventClose(false);
    exit(0);
  }

  @override
  void dispose() {
    _poller?.cancel();
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Row(children: [
          Icon(Icons.theater_comedy, color: Color(0xFFF59E0B)),
          SizedBox(width: 8),
          Text('Understudy'),
        ]),
        actions: [
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _refresh,
          ),
          IconButton(
            icon: const Icon(Icons.power_settings_new),
            tooltip: 'Quit — kill switch (stops the engine)',
            onPressed: _quit,
          ),
        ],
      ),
      // Tabs are always reachable: engine trouble is a card on the
      // Status tab (and a note on the Tailscale tab), never a screen
      // that replaces every tab.
      body: IndexedStack(
              index: _tab,
              children: [
                _StatusTab(
                  status: _status,
                  monitors: _monitors,
                  ts: _ts,
                  enginePort: _api?.info.port,
                  api: _api,
                  tokenRevealed: _tokenRevealed,
                  onReveal: () =>
                      setState(() => _tokenRevealed = !_tokenRevealed),
                  onRotate: _rotateToken,
                  onQuit: _quit,
                  notice: _notice,
                  engineState: _engineState,
                  engineFailure: _engineFailure,
                  onStartEngine: () => _ensureEngine(manual: true),
                  update: _update,
                  updateChecking: _updateChecking,
                  updateMessage: _updateMessage,
                  onCheckUpdate: _manualUpdateCheck,
                  onDownloadInstall: _downloadAndInstall,
                ),
                _LogTab(entries: _log),
                _SetupTab(
                  ts: _ts,
                  api: _api,
                  onNotice: (s) => setState(() => _notice = s),
                  onStartEngine: () => _ensureEngine(manual: true),
                ),
              ],
            ),
      bottomNavigationBar: BottomNavigationBar(
        currentIndex: _tab,
        onTap: (i) => setState(() => _tab = i),
        items: const [
          BottomNavigationBarItem(icon: Icon(Icons.monitor), label: 'Status'),
          BottomNavigationBarItem(
              icon: Icon(Icons.receipt_long), label: 'Action log'),
          BottomNavigationBarItem(
              icon: Icon(Icons.vpn_key), label: 'Tailscale'),
        ],
      ),
    );
  }
}

class _StatusTab extends StatelessWidget {
  const _StatusTab({
    required this.status,
    required this.monitors,
    required this.ts,
    required this.enginePort,
    required this.api,
    required this.tokenRevealed,
    required this.onReveal,
    required this.onRotate,
    required this.onQuit,
    required this.notice,
    required this.engineState,
    required this.engineFailure,
    required this.onStartEngine,
    required this.update,
    required this.updateChecking,
    required this.updateMessage,
    required this.onCheckUpdate,
    required this.onDownloadInstall,
  });

  final Map<String, dynamic>? status;
  final List<dynamic> monitors;
  final Map<String, dynamic>? ts;
  final int? enginePort;
  final ApiClient? api;
  final bool tokenRevealed;
  final VoidCallback onReveal;
  final VoidCallback onRotate;
  final VoidCallback onQuit;
  final String? notice;
  final String engineState;
  final String? engineFailure;
  final VoidCallback onStartEngine;
  final UpdateInfo? update;
  final bool updateChecking;
  final String? updateMessage;
  final VoidCallback onCheckUpdate;
  final VoidCallback onDownloadInstall;

  @override
  Widget build(BuildContext context) {
    final active = status?['session_active'] == true;
    // Local copy so null checks promote (fields do not promote).
    final upd = update;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        if (engineState != 'running') ...[
          Card(
            child: ListTile(
              leading: engineState == 'starting'
                  ? const SizedBox(
                      width: 18,
                      height: 18,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Icon(Icons.error, color: Colors.red, size: 18),
              title: Text(engineState == 'starting'
                  ? 'Starting engine…'
                  : 'Engine not running'),
              subtitle:
                  engineFailure == null ? null : Text(engineFailure!),
              trailing: engineState == 'starting'
                  ? null
                  : TextButton(
                      onPressed: onStartEngine,
                      child: const Text('Start engine'),
                    ),
            ),
          ),
          const SizedBox(height: 8),
        ],
        Card(
          child: ListTile(
            leading: const Icon(Icons.system_update),
            title: Text(upd != null
                ? 'Update available: v${upd.version}'
                : updateChecking
                    ? 'Checking for updates…'
                    : 'Updates'),
            subtitle: updateMessage == null
                ? const Text(
                    'Understudy checks for updates at most once a day.')
                : Text(updateMessage!),
          ),
        ),
        if (upd != null) ...[
          const SizedBox(height: 8),
          ConstrainedBox(
            constraints: const BoxConstraints(maxHeight: 200),
            child: SingleChildScrollView(
              child: SelectableText(
                  upd.notes.isEmpty ? 'No release notes.' : upd.notes),
            ),
          ),
          const SizedBox(height: 8),
        ],
        Row(children: [
          OutlinedButton.icon(
            onPressed: updateChecking ? null : onCheckUpdate,
            icon: const Icon(Icons.refresh),
            label: const Text('Check for updates'),
          ),
          if (upd != null) ...[
            const SizedBox(width: 8),
            FilledButton.icon(
              onPressed: onDownloadInstall,
              icon: const Icon(Icons.download),
              label: const Text('Download & install'),
            ),
          ],
        ]),
        const SizedBox(height: 8),
        Card(
          child: ListTile(
            leading: Icon(
              Icons.circle,
              color: active ? Colors.green : Colors.grey,
              size: 18,
            ),
            title: Text(active
                ? 'A controller session is active'
                : 'No controller session in the last 30s'),
            subtitle: Text(
              'Engine v${status?['version'] ?? '…'} · '
              '${status?['requests'] ?? 0} requests served · '
              'uptime ${status?['uptime_s'] ?? 0}s',
            ),
          ),
        ),
        const SizedBox(height: 8),
        // Controller connection: the address a controller needs to
        // reach this PC — Tailscale device name + IP + control port
        // in one copyable line. This is deliberately separate from the
        // pairing-token reveal below: the address and the token are
        // never shown together in one copyable block, so they cannot
        // be pasted out together by accident. When Tailscale is not
        // connected, this says so plainly instead of showing a stale
        // or empty address.
        Builder(builder: (context) {
          // Local copies so null checks promote (fields do not).
          final tsLocal = ts;
          final port = enginePort;
          final ip = tsLocal?['ip'];
          final connected = tsLocal != null &&
              tsLocal['logged_in'] == true &&
              ip is String &&
              ip.isNotEmpty &&
              port != null;
          // The extra null checks promote the locals for use below
          // (Dart does not promote through the `connected` boolean).
          if (!connected || tsLocal == null || port == null) {
            return const Card(
              child: ListTile(
                leading: Icon(Icons.link_off),
                title: Text('Controller connection'),
                subtitle: Text('Not connected to Tailscale — connect on '
                    'the Tailscale tab'),
              ),
            );
          }
          final address = 'http://$ip:$port';
          final deviceName =
              tsLocal['device_name'] as String? ?? 'unknown';
          return Card(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                ListTile(
                  leading: const Icon(Icons.link, color: Colors.green),
                  title: const Text('Controller connection'),
                  subtitle: Text('Device $deviceName · Tailscale IP $ip · '
                      'port $port'),
                ),
                Padding(
                  padding:
                      const EdgeInsets.fromLTRB(16, 0, 16, 8),
                  child: Row(
                    children: [
                      Expanded(child: SelectableText(address)),
                      IconButton(
                        icon: const Icon(Icons.copy),
                        tooltip: 'Copy controller address',
                        onPressed: () {
                          Clipboard.setData(
                              ClipboardData(text: address));
                          ScaffoldMessenger.of(context).showSnackBar(
                            const SnackBar(
                              content: Text(
                                  'Controller address copied'),
                            ),
                          );
                        },
                      ),
                    ],
                  ),
                ),
              ],
            ),
          );
        }),
        const SizedBox(height: 8),
        const Text('Monitors shared',
            style: TextStyle(fontSize: 16, fontWeight: FontWeight.bold)),
        for (final m in monitors)
          ListTile(
            leading: const Icon(Icons.display_settings),
            title: Text(
                'Monitor ${m['id']}${m['is_primary'] == true ? ' (primary)' : ''}'),
            subtitle: Text(
                '${m['width']}×${m['height']} at (${m['x']}, ${m['y']}), '
                'scale ${m['scale']}'),
          ),
        const SizedBox(height: 8),
        const Text(
          'The control API is the way this PC is operated — screenshots '
          'in, reasoned actions out. This app only watches, logs, and '
          'stops it.',
          style: TextStyle(color: Colors.grey),
        ),
        const SizedBox(height: 8),
        if (notice != null)
          Card(child: ListTile(title: Text(notice!))),
        const SizedBox(height: 8),
        OutlinedButton.icon(
          onPressed: onReveal,
          icon: const Icon(Icons.key),
          label: Text(tokenRevealed ? 'Hide pairing token' : 'Reveal pairing token'),
        ),
        if (tokenRevealed)
          FutureBuilder<EngineInfo?>(
            future: readEngineInfo(),
            builder: (context, snap) {
              final token = snap.data?.token ?? '…';
              return SelectableText(
                'Bearer token (paste into your controller once, then keep it private):\n$token',
              );
            },
          ),
        const SizedBox(height: 8),
        OutlinedButton.icon(
          onPressed: onRotate,
          icon: const Icon(Icons.refresh),
          label: const Text('Rotate pairing token'),
        ),
        const Text(
          'Rotating generates a fresh token and kills the old one '
          'immediately — use it if a token ever leaks. The new token '
          'appears above after rotation.',
          style: TextStyle(color: Colors.grey),
        ),
        const SizedBox(height: 16),
        FilledButton.icon(
          onPressed: onQuit,
          icon: const Icon(Icons.power_settings_new),
          label: const Text('Quit — kill switch (stops the engine)'),
        ),
      ],
    );
  }
}

class _LogTab extends StatelessWidget {
  const _LogTab({required this.entries});
  final List<dynamic> entries;

  @override
  Widget build(BuildContext context) {
    if (entries.isEmpty) {
      return const Center(child: Text('No actions logged yet.'));
    }
    return ListView.builder(
      itemCount: entries.length,
      itemBuilder: (context, i) {
        final e = entries[entries.length - 1 - i] as Map<String, dynamic>;
        final ts = DateTime.fromMillisecondsSinceEpoch(
            ((e['ts'] as num?)?.toInt() ?? 0) * 1000);
        return ListTile(
          leading: Icon(
            e['ok'] == true ? Icons.check_circle : Icons.error,
            color: e['ok'] == true ? Colors.green : Colors.red,
            size: 18,
          ),
          title: Text('${e['action']}'),
          subtitle: Text('${e['detail'] ?? ''}'),
          trailing: Text(
              '${ts.hour.toString().padLeft(2, '0')}:${ts.minute.toString().padLeft(2, '0')}:${ts.second.toString().padLeft(2, '0')}'),
        );
      },
    );
  }
}

class _SetupTab extends StatefulWidget {
  const _SetupTab(
      {required this.ts,
      required this.api,
      required this.onNotice,
      required this.onStartEngine});
  final Map<String, dynamic>? ts;
  final ApiClient? api;
  final ValueChanged<String> onNotice;
  final VoidCallback onStartEngine;

  @override
  State<_SetupTab> createState() => _SetupTabState();
}

class _SetupTabState extends State<_SetupTab> {
  final _version = TextEditingController();
  final _authKey = TextEditingController();
  bool _busy = false;
  String? _setupMessage;

  /// Results show HERE, on the Tailscale tab where the buttons live —
  /// v0.1.0 only surfaced them as a notice on the Status tab, so an
  /// install failure looked like nothing happening at all.
  void _say(String message) {
    widget.onNotice(message);
    if (mounted) setState(() => _setupMessage = message);
  }

  Future<void> _install() async {
    final api = widget.api;
    if (api == null) return;
    setState(() {
      _busy = true;
      _setupMessage =
          'Installing… a Windows admin prompt will appear — approve it. '
          'This runs hidden (no console window) and can take a minute.';
    });
    try {
      final res = await api.tailscaleInstall(_version.text.trim());
      _say('Tailscale installed and verified: ${res['installed_version']}');
    } catch (e) {
      _say('Install failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _login([String? key]) async {
    final api = widget.api;
    if (api == null) return;
    setState(() => _busy = true);
    try {
      await api.tailscaleLogin(key);
      _say(key == null
          ? 'Browser sign-in started — complete it in your browser, then come back.'
          : 'Login with auth key started.');
    } catch (e) {
      _say('Login failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final ts = widget.ts;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        ListTile(
          leading: const Icon(Icons.vpn_key),
          title: Text(ts == null
              ? 'Tailscale status unknown'
              : ts['installed'] == true
                  ? 'Tailscale installed'
                  : 'Tailscale not installed'),
          subtitle: Text(ts?['ip'] != null
              ? 'Connected as ${ts!['ip']}'
                  '${ts!['device_name'] != null ? ' · ${ts!['device_name']}' : ''}'
              : 'Not connected yet'),
        ),
        if (widget.api == null) ...[
          Card(
            child: ListTile(
              leading: const Icon(Icons.info),
              title: const Text('Engine not running yet'),
              subtitle: const Text(
                  'Tailscale setup runs through the engine. Start the '
                  'engine first — it launches from this app automatically, '
                  'no manual step needed.'),
              trailing: TextButton(
                onPressed: widget.onStartEngine,
                child: const Text('Start engine'),
              ),
            ),
          ),
          const SizedBox(height: 8),
        ],
        const Text(
          'First launch: if Tailscale is missing, this app downloads the '
          'official installer and runs it hidden — one Windows admin prompt '
          'will appear; approve it. Then sign in once (browser, or an auth '
          'key from your Tailscale admin console). After that the connection '
          'is headless. The result of every step shows below the buttons.',
          style: TextStyle(color: Colors.grey),
        ),
        if (_setupMessage != null) ...[
          const SizedBox(height: 8),
          Card(child: ListTile(title: Text(_setupMessage!))),
        ],
        const SizedBox(height: 12),
        TextField(
          controller: _version,
          decoration: const InputDecoration(
            labelText: 'Tailscale version (blank = latest stable)',
            border: OutlineInputBorder(),
          ),
        ),
        const SizedBox(height: 8),
        FilledButton(
          onPressed: _busy ? null : _install,
          child: const Text('Install Tailscale'),
        ),
        const SizedBox(height: 16),
        TextField(
          controller: _authKey,
          decoration: const InputDecoration(
            labelText: 'Auth key (optional — leave blank for browser sign-in)',
            border: OutlineInputBorder(),
          ),
          obscureText: true,
        ),
        const SizedBox(height: 8),
        Row(children: [
          Expanded(
            child: FilledButton(
              onPressed: _busy ? null : () => _login(),
              child: const Text('Sign in via browser'),
            ),
          ),
          const SizedBox(width: 8),
          Expanded(
            child: OutlinedButton(
              onPressed: _busy
                  ? null
                  : () => _login(
                      _authKey.text.trim().isEmpty ? null : _authKey.text.trim()),
              child: const Text('Sign in with auth key'),
            ),
          ),
        ]),
      ],
    );
  }
}
