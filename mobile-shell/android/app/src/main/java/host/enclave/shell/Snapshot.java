package host.enclave.shell;

import android.content.Context;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;

import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.URI;
import java.util.HashMap;
import java.util.Map;

/**
 * Serves the app's bundled UI snapshot (assets/appsnapshot, written by
 * snapshot.mjs at build time) in place of the network: the webview browses
 * the app's REAL origin, but every GET whose path is in the snapshot
 * manifest answers from the APK. Everything else - the API POSTs, the
 * streams, any path the snapshot does not carry - passes through untouched.
 * No snapshot in the APK means no interception at all.
 */
final class Snapshot {
    private static boolean loaded = false;
    private static String origin = null;
    private static final Map<String, String[]> files = new HashMap<>(); // path -> [assetFile, mime]

    private Snapshot() {}

    private static synchronized void load(Context ctx) {
        if (loaded) return;
        loaded = true;
        try (InputStream in = ctx.getAssets().open("appsnapshot/manifest.json")) {
            StringBuilder sb = new StringBuilder();
            BufferedReader r = new BufferedReader(new InputStreamReader(in, "UTF-8"));
            for (String line; (line = r.readLine()) != null; ) sb.append(line);
            JSONObject man = new JSONObject(sb.toString());
            String o = man.optString("origin", "");
            JSONObject fs = man.optJSONObject("files");
            if (o.startsWith("https://") && fs != null) {
                for (java.util.Iterator<String> it = fs.keys(); it.hasNext(); ) {
                    String path = it.next();
                    JSONObject f = fs.getJSONObject(path);
                    files.put(path, new String[] { f.getString("file"), f.getString("mime") });
                }
                origin = o;
            }
        } catch (Exception ignored) {
            // no snapshot bundled (or unreadable): plain remote-loading shell
        }
    }

    static WebResourceResponse serve(Context ctx, WebResourceRequest req) {
        load(ctx);
        if (origin == null || !"GET".equalsIgnoreCase(req.getMethod())) return null;
        try {
            URI u = new URI(req.getUrl().toString());
            String reqOrigin = u.getScheme() + "://" + u.getRawAuthority();
            if (!origin.equalsIgnoreCase(reqOrigin)) return null;
            String path = u.getPath();
            if (path == null || path.isEmpty()) path = "/";
            String[] hit = files.get(path);
            if (hit == null) return null;
            String mime = hit[1];
            String enc = null;
            int semi = mime.indexOf(';');
            if (semi >= 0) mime = mime.substring(0, semi).trim();
            if (mime.startsWith("text/") || mime.endsWith("json") || mime.endsWith("javascript")
                    || mime.endsWith("xml") || mime.endsWith("svg+xml")) enc = "utf-8";
            InputStream body = ctx.getAssets().open("appsnapshot/" + hit[0]);
            return new WebResourceResponse(mime, enc, body);
        } catch (Exception e) {
            return null; // any doubt: let the network answer
        }
    }
}
