package host.enclave.shell;

import android.os.Bundle;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebView;

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
        });
    }
}
