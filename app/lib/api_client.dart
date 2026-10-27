/// Minimal client for the Understudy engine's control API.
///
/// The engine is a local process (spawned by this app on launch) serving
/// HTTP on the Tailscale interface or loopback. This client reads the
/// connection info (`connection.json`) and bearer token (`auth.token`)
/// from the engine's data directory, which this app shares because it runs
/// as the same user. The token is never logged or sent anywhere except as
/// the Authorization header to the engine itself.
///
/// Only the monitoring/setup endpoints are used here: status, monitors,
/// log, Tailscale setup, and shutdown (the kill switch). The full control
/// endpoints (input, apps, files, cmd, screenshots) belong to the API's
/// primary consumer — the AI operator — and are documented in
/// CONTROL_API.md, not driven from this UI.
library;

import 'dart:convert';
import 'dart:io';

/// Where the engine keeps its token, log, and connection info.
Future<Directory> dataDir() async {
  if (Platform.isWindows) {
    final appData = Platform.environment['APPDATA'];
    if (appData != null) {
      return Directory('$appData\\Understudy');
    }
    return Directory('Understudy');
  }
  final home = Platform.environment['HOME'];
  if (home != null) return Directory('$home/.understudy');
  return Directory('.understudy');
}

class EngineInfo {
  EngineInfo({required this.ip, required this.port, required this.token});
  final String ip;
  final int port;
  final String token;

  String get base => 'http://$ip:$port';
}

/// Read connection.json + auth.token written by the engine. Returns null
/// when the engine has not started yet (the UI keeps polling).
Future<EngineInfo?> readEngineInfo() async {
  try {
    final dir = await dataDir();
    final connFile = File('${dir.path}/connection.json');
    final tokenFile = File('${dir.path}/auth.token');
    if (!await connFile.exists() || !await tokenFile.exists()) return null;
    final conn =
        jsonDecode(await connFile.readAsString()) as Map<String, dynamic>;
    final token = (await tokenFile.readAsString()).trim();
    if (token.isEmpty) return null;
    return EngineInfo(
      ip: conn['ip'] as String? ?? '127.0.0.1',
      port: (conn['port'] as num?)?.toInt() ?? 18769,
      token: token,
    );
  } catch (_) {
    return null;
  }
}

class ApiClient {
  ApiClient(this.info);
  final EngineInfo info;

  Future<Map<String, dynamic>> _get(String path) async {
    final client = HttpClient();
    try {
      final req = await client.getUrl(Uri.parse('${info.base}$path'));
      req.headers.set('Authorization', 'Bearer ${info.token}');
      final resp = await req.close();
      final body = await resp.transform(utf8.decoder).join();
      if (resp.statusCode != 200) {
        throw Exception('GET $path failed (${resp.statusCode}): $body');
      }
      return jsonDecode(body) as Map<String, dynamic>;
    } finally {
      client.close();
    }
  }

  Future<Map<String, dynamic>> _post(
      String path, Map<String, dynamic> body) async {
    final client = HttpClient();
    try {
      final req = await client.postUrl(Uri.parse('${info.base}$path'));
      req.headers.set('Authorization', 'Bearer ${info.token}');
      req.headers.contentType = ContentType.json;
      req.write(jsonEncode(body));
      final resp = await req.close();
      final text = await resp.transform(utf8.decoder).join();
      if (resp.statusCode != 200) {
        throw Exception('POST $path failed (${resp.statusCode}): $text');
      }
      return jsonDecode(text) as Map<String, dynamic>;
    } finally {
      client.close();
    }
  }

  Future<Map<String, dynamic>> status() => _get('/status');
  Future<Map<String, dynamic>> monitors() => _get('/monitors');
  Future<Map<String, dynamic>> log({int limit = 200}) =>
      _get('/log?limit=$limit');
  Future<Map<String, dynamic>> tailscaleStatus() =>
      _get('/tailscale/status');
  Future<Map<String, dynamic>> tailscaleInstall(String version) =>
      _post('/tailscale/install', {'version': version});
  Future<Map<String, dynamic>> tailscaleLogin([String? authKey]) =>
      _post('/tailscale/login', {if (authKey != null) 'authkey': authKey});

  /// The kill switch: stops the engine (all listeners close), after which
  /// this app exits too.
  Future<void> shutdown() async {
    try {
      await _post('/shutdown', {});
    } catch (_) {
      // The engine may close the connection as it stops; that is success.
    }
  }
}
