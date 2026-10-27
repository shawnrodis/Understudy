/// Self-update for the Understudy tray app, following the pattern of the
/// owner's other Windows apps: a launch check against the GitHub
/// Releases API, throttled to at most once per 24h and silent on failure
/// or when no newer release exists, plus a manual check from the UI.
/// Nothing is ever installed silently: when a newer release exists the
/// UI shows its notes and offers Download & install, which downloads
/// the new installer and launches it (one UAC prompt expected) after
/// this app quits, so the installer can replace the running files.
library;

import 'dart:convert';
import 'dart:io';

import 'api_client.dart';

/// Bump this with every release (it must match the release tag).
const kAppVersion = '0.1.3';

const _repo = 'shawnrodis/Understudy';

class UpdateInfo {
  UpdateInfo({
    required this.available,
    required this.version,
    required this.notes,
    required this.downloadUrl,
    this.error,
  });
  final bool available;
  final String version;
  final String notes;
  final String downloadUrl;
  final String? error;
}

/// Compare two dotted version strings numerically ("1.10" > "1.9").
/// Non-numeric segments count as 0; unparseable tags are never newer.
bool _isNewer(String candidate, String current) {
  List<int> parts(String v) => v
      .split('.')
      .map((p) => int.tryParse(p) ?? 0)
      .toList();
  final a = parts(candidate);
  final b = parts(current);
  for (var i = 0; i < a.length || i < b.length; i++) {
    final x = i < a.length ? a[i] : 0;
    final y = i < b.length ? b[i] : 0;
    if (x != y) return x > y;
  }
  return false;
}

Future<File> _stampFile() async {
  final dir = await dataDir();
  return File('${dir.path}/update_check.txt');
}

/// Check for a newer release. Returns null only when an automatic check
/// is throttled (a check ran within the last 24h) — the caller stays
/// silent in that case. A manual check always returns an UpdateInfo
/// (with [UpdateInfo.error] set when the check itself failed).
Future<UpdateInfo?> checkForUpdate({bool manual = false}) async {
  if (!manual) {
    try {
      final stamp = await _stampFile();
      if (await stamp.exists()) {
        final last = DateTime.tryParse(await stamp.readAsString());
        if (last != null &&
            DateTime.now().difference(last) <
                const Duration(hours: 24)) {
          return null;
        }
      }
      await stamp.writeAsString(DateTime.now().toIso8601String());
    } catch (_) {
      // A broken stamp file must never block the check; fall through.
    }
  }
  final client = HttpClient();
  try {
    final req = await client.getUrl(Uri.parse(
        'https://api.github.com/repos/$_repo/releases/latest'));
    req.headers.set('User-Agent', 'Understudy');
    req.headers.set('Accept', 'application/vnd.github+json');
    final resp = await req.close();
    final body = await resp.transform(utf8.decoder).join();
    if (resp.statusCode != 200) {
      return UpdateInfo(
        available: false,
        version: kAppVersion,
        notes: '',
        downloadUrl: '',
        error: 'GitHub API returned ${resp.statusCode}',
      );
    }
    final decoded = jsonDecode(body) as Map<String, dynamic>;
    final tag = (decoded['tag_name'] as String? ?? '').trim();
    final version = tag.startsWith('v') ? tag.substring(1) : tag;
    final notes = decoded['body'] as String? ?? '';
    String downloadUrl = '';
    final assets = decoded['assets'] as List<dynamic>? ?? [];
    for (final a in assets) {
      final name = (a as Map<String, dynamic>)['name'] as String? ?? '';
      if (name.toLowerCase().endsWith('.exe')) {
        downloadUrl =
            a['browser_download_url'] as String? ?? '';
        break;
      }
    }
    return UpdateInfo(
      available: version.isNotEmpty && _isNewer(version, kAppVersion),
      version: version.isEmpty ? kAppVersion : version,
      notes: notes,
      downloadUrl: downloadUrl,
    );
  } catch (e) {
    return UpdateInfo(
      available: false,
      version: kAppVersion,
      notes: '',
      downloadUrl: '',
      error: '$e',
    );
  } finally {
    client.close();
  }
}

/// Download the installer to the temp dir; returns its full path.
/// Throws on a non-200 response.
Future<String> downloadInstaller(String url, String version) async {
  final client = HttpClient();
  try {
    final req = await client.getUrl(Uri.parse(url));
    req.headers.set('User-Agent', 'Understudy');
    final resp = await req.close();
    if (resp.statusCode != 200) {
      throw Exception('Download failed (HTTP ${resp.statusCode})');
    }
    final bytes = await resp.fold<List<int>>(
        <int>[], (acc, chunk) => acc..addAll(chunk));
    final path =
        '${Directory.systemTemp.path}/Understudy_Setup_$version.exe';
    await File(path).writeAsBytes(bytes);
    return path;
  } finally {
    client.close();
  }
}

String _psQuote(String s) => "'${s.replaceAll("'", "''")}'";

/// Launch the installer after this app exits. A hidden PowerShell waits
/// for our PID to disappear, then starts the installer elevated (one
/// UAC prompt, expected). The caller must quit immediately after this
/// returns — the installer cannot replace files while this app runs.
Future<void> launchInstallerAfterQuit(String installerPath, int pid) async {
  await Process.start(
    'powershell.exe',
    [
      '-NoProfile',
      '-WindowStyle',
      'Hidden',
      '-Command',
      'Wait-Process -Id $pid -ErrorAction SilentlyContinue; '
          'Start-Process -FilePath ${_psQuote(installerPath)} -Verb RunAs',
    ],
    mode: ProcessStartMode.detached,
  );
}
