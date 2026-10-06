package host.enclave.shell;

import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebView;

import androidx.browser.customtabs.CustomTabsIntent;

import com.getcapacitor.BridgeActivity;
import com.getcapacitor.BridgeWebViewClient;

public class MainActivity extends BridgeActivity {
    @Override
    public void onStart() {
        super.onStart();
        // Prepackaged builds ship the app's UI in the APK (assets/appsnapshot):
        // the webview still browses the real origin - API calls, cookies and
        // streams stay natively same-origin - but every snapshotted GET is
        // answered from the bundle instead of the network. Builds without a
        // snapshot fall through to Capacitor's own client behaviour untouched.
        WebView wv = this.bridge.getWebView();
        wv.setWebViewClient(new BridgeWebViewClient(this.bridge) {
            @Override
            public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest request) {
                WebResourceResponse local = Snapshot.serve(MainActivity.this, request);
                return local != null ? local : super.shouldInterceptRequest(view, request);
            }

            // Sign in with Enclave: enclave.host never loads in this webview
            // (it is not in allowNavigation). Open it in a Custom Tab instead of
            // a separate browser app, so the browser's existing enclave.host
            // session passes straight through and the tab closes when the
            // sign-in comes back through the /sso-app app link (onNewIntent).
            @Override
            public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                Uri u = request.getUrl();
                if (isEnclaveSignIn(u)) {
                    try {
                        new CustomTabsIntent.Builder().setShowTitle(true).build().launchUrl(MainActivity.this, u);
                        return true;
                    } catch (RuntimeException e) {
                        // no Custom Tabs provider: fall through to the default
                        // (Capacitor hands the URL to the system browser)
                    }
                }
                return super.shouldOverrideUrlLoading(view, request);
            }
        });
    }

    private static boolean isEnclaveSignIn(Uri u) {
        return u != null && "https".equals(u.getScheme()) && "enclave.host".equals(u.getHost())
            && u.getPath() != null && u.getPath().startsWith("/sso/");
    }

    // A sign-in returning through the verified app link while the app runs
    // (singleTask: this activity comes back to the front and the Custom Tab
    // above it is closed). The fragment goes to the page that is already open
    // - but only if that page IS the verified app: on the splash, a failed
    // verification or the pairing screen, the token is dropped (it is useless
    // without the page's state anyway, and nothing may skip the verify gate).
    // A cold start never gets here: the splash reads the launch URL instead.
    @Override
    protected void onNewIntent(Intent intent) {
        if (deliverSignIn(intent)) {
            setIntent(intent);
            return;
        }
        super.onNewIntent(intent);
    }

    private boolean deliverSignIn(Intent intent) {
        Uri u = intent == null ? null : intent.getData();
        if (u == null || !"https".equals(u.getScheme()) || u.getPath() == null) return false;
        if (!(u.getPath().equals("/sso-app") || u.getPath().startsWith("/sso-app/"))) return false;
        String frag = u.getEncodedFragment();
        if (frag == null || !(frag.startsWith("sso=") || frag.contains("&sso="))) return true; // ours, nothing to carry
        WebView wv = this.bridge == null ? null : this.bridge.getWebView();
        if (wv == null) return true;
        String origin = u.getScheme() + "://" + u.getEncodedAuthority();
        String cur = wv.getUrl();
        if (cur == null || !(cur.equals(origin) || cur.startsWith(origin + "/"))) return true;
        int h = cur.indexOf('#');
        String page = h >= 0 ? cur.substring(0, h) : cur;
        // same document: only the hash changes, and the page's hashchange
        // listener runs the state-checked acceptance; otherwise it boots with it
        wv.loadUrl(page + "#" + frag);
        return true;
    }
}
