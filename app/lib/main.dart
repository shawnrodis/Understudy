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

/// Loopback port the single-instance guard listens on. Unrelated to the
/// engine's control port (18769): this one exists only so a second app
/// launch can find the first and hand off to it.
const _singleInstancePort = 18771;

void main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await windowManager.ensureInitialized();
  await _enforceSingleInstance();
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
  String? _error;
  String? _notice;
  bool _engineStarting = false;
  bool _tokenRevealed = false;
  Timer? _poller;

  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    _initTray();
    _ensureEngine();
    _poller = Timer.periodic(const Duration(seconds: 3), (_) => _refresh());
  }

  Future<void> _initTray() async {
    try {
      final exeDir = File(Platform.resolvedExecutable).parent;
      final ico = File('${exeDir.path}/icon.ico');
      if (await ico.exists()) {
        await trayManager.setIcon(ico.path);
      }
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
      // Tray is a convenience; the app must work without it.
    }
  }

  @override
  void onWindowClose() async {
    // X tucks to tray; Quit is the real exit (the kill switch).
    await windowManager.hide();
  }

  Future<void> _ensureEngine() async {
    if (_engineStarting) return;
    _engineStarting = true;
    try {
      final info = await readEngineInfo();
      if (info != null) {
        _api = ApiClient(info);
        await _refresh();
        return;
      }
      final exeDir = File(Platform.resolvedExecutable).parent;
      final engineExe = File('${exeDir.path}/understudy-engine.exe');
      final dir = await dataDir();
      if (await engineExe.exists()) {
        _engine = await Process.start(
          engineExe.path,
          ['--data-dir', dir.path],
          mode: ProcessStartMode.detached,
        );
        // Poll until the engine writes connection.json.
        for (var i = 0; i < 20; i++) {
          await Future<void>.delayed(const Duration(milliseconds: 500));
          final found = await readEngineInfo();
          if (found != null) {
            _api = ApiClient(found);
            break;
          }
        }
      }
      await _refresh();
    } catch (e) {
      setState(() => _error = 'Engine start failed: $e');
    } finally {
      _engineStarting = false;
    }
  }

  Future<void> _refresh() async {
    final api = _api;
    if (api == null) {
      final info = await readEngineInfo();
      if (info != null) {
        _api = ApiClient(info);
        return _refresh();
      }
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
        _error = null;
      });
    } catch (e) {
      if (mounted) setState(() => _error = 'Engine unreachable: $e');
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
      body: _error != null && _status == null
          ? _SetupNeeded(error: _error!)
          : IndexedStack(
              index: _tab,
              children: [
                _StatusTab(
                  status: _status,
                  monitors: _monitors,
                  api: _api,
                  tokenRevealed: _tokenRevealed,
                  onReveal: () =>
                      setState(() => _tokenRevealed = !_tokenRevealed),
                  onRotate: _rotateToken,
                  onQuit: _quit,
                  notice: _notice,
                ),
                _LogTab(entries: _log),
                _SetupTab(
                  ts: _ts,
                  api: _api,
                  onNotice: (s) => setState(() => _notice = s),
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

class _SetupNeeded extends StatelessWidget {
  const _SetupNeeded({required this.error});
  final String error;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Icon(Icons.hourglass_top, size: 48),
            const SizedBox(height: 16),
            const Text(
              'Waiting for the engine…',
              style: TextStyle(fontSize: 18, fontWeight: FontWeight.bold),
            ),
            const SizedBox(height: 8),
            Text(error, textAlign: TextAlign.center),
            const SizedBox(height: 8),
            const Text(
              'If the engine is not installed next to this app, run '
              'understudy-engine.exe once from the install folder.',
              textAlign: TextAlign.center,
            ),
          ],
        ),
      ),
    );
  }
}

class _StatusTab extends StatelessWidget {
  const _StatusTab({
    required this.status,
    required this.monitors,
    required this.api,
    required this.tokenRevealed,
    required this.onReveal,
    required this.onRotate,
    required this.onQuit,
    required this.notice,
  });

  final Map<String, dynamic>? status;
  final List<dynamic> monitors;
  final ApiClient? api;
  final bool tokenRevealed;
  final VoidCallback onReveal;
  final VoidCallback onRotate;
  final VoidCallback onQuit;
  final String? notice;

  @override
  Widget build(BuildContext context) {
    final active = status?['session_active'] == true;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
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
      {required this.ts, required this.api, required this.onNotice});
  final Map<String, dynamic>? ts;
  final ApiClient? api;
  final ValueChanged<String> onNotice;

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
              : 'Not connected yet'),
        ),
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
