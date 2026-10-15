package io.github.storytold.cadcraft;

import android.app.Activity;
import android.app.ActivityManager;
import android.app.AlertDialog;
import android.app.NativeActivity;
import android.content.ClipData;
import android.content.Intent;
import android.content.res.Configuration;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;
import android.text.InputFilter;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.webkit.MimeTypeMap;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.TextView;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.ref.WeakReference;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Locale;
import java.util.UUID;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import org.json.JSONObject;

/** Lifecycle and document-provider glue; CAD behaviour stays in the shared Rust engine. */
public final class MainActivity extends NativeActivity {
    static { System.loadLibrary("cadcraft_android"); }
    private static native void nativeWake(int kind);
    private static volatile WeakReference<MainActivity> current = new WeakReference<>(null);
    private static volatile JSONObject labels = new JSONObject();
    private static final int OPEN = 41, EXPORT = 42, COMMAND = 43, NAME = 44, LINK = 45, PROJECT = 46;
    private final ConcurrentLinkedQueue<String> imports = new ConcurrentLinkedQueue<>();
    private final ConcurrentLinkedQueue<String> notices = new ConcurrentLinkedQueue<>();
    private final ConcurrentLinkedQueue<String> commands = new ConcurrentLinkedQueue<>();
    private final ConcurrentLinkedQueue<String> events = new ConcurrentLinkedQueue<>();
    private final ExecutorService files = Executors.newSingleThreadExecutor();
    private boolean picking;
    private File exporting;

    public static MainActivity currentActivity() { return current.get(); }
    private static String tr(String key) { return labels.optString(key, key); }
    public void setLabels(String json) { try { labels = new JSONObject(json); } catch (Exception ignored) {} }
    private static void wake(int kind) { try { nativeWake(kind); } catch (UnsatisfiedLinkError ignored) {} }
    private void notice(String text) { notices.offer(text); wake(0); }
    private void notice(String action, Exception error) { notice(tr(action) + ": " + (error.getMessage() == null ? tr("File operation failed") : error.getMessage())); }
    private void event(String kind, String value) {
        try { events.offer(new JSONObject().put("kind", kind).put("value", value).toString()); wake(0); } catch (Exception ignored) {}
    }
    private void imported(File path) { imports.offer(path.getAbsolutePath()); wake(0); }
    public String takeImportedPath() { return imports.poll(); }
    public String takeNotice() { return notices.poll(); }
    public String takeCommand() { return commands.poll(); }
    public String takeEvent() { return events.poll(); }
    public String interfaceLocale() { return Locale.getDefault().toLanguageTag(); }

    @Override public void onCreate(Bundle state) {
        current = new WeakReference<>(this);
        super.onCreate(state);
        drawingScreen();
        openIntent(getIntent());
    }
    @Override protected void onNewIntent(Intent intent) { super.onNewIntent(intent); setIntent(intent); openIntent(intent); }
    @Override protected void onPause() { wake(1); super.onPause(); }
    @Override protected void onStop() { wake(1); event("trim", ""); super.onStop(); }
    @Override public void onTrimMemory(int level) { super.onTrimMemory(level); if (level >= TRIM_MEMORY_UI_HIDDEN) event("trim", ""); }
    @Override public void onConfigurationChanged(Configuration configuration) {
        super.onConfigurationChanged(configuration); event("locale", interfaceLocale());
    }
    @Override public void onBackPressed() { event("back", ""); }
    public void background() { runOnUiThread(() -> moveTaskToBack(true)); }
    @Override public void onWindowFocusChanged(boolean focused) { super.onWindowFocusChanged(focused); if (focused) drawingScreen(); }
    private void drawingScreen() {
        if (Build.VERSION.SDK_INT >= 30) {
            WindowInsetsController controller = getWindow().getInsetsController();
            if (controller != null) { controller.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE); controller.hide(WindowInsets.Type.systemBars()); }
        } else getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_FULLSCREEN | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
    }
    public long memoryBudget() {
        ActivityManager manager = getSystemService(ActivityManager.class);
        ActivityManager.MemoryInfo info = new ActivityManager.MemoryInfo();
        if (manager == null) return 64L << 20;
        manager.getMemoryInfo(info);
        return Math.max(32L << 20, Math.min(256L << 20, info.availMem / 16));
    }
    public String memoryState() {
        ActivityManager manager = getSystemService(ActivityManager.class);
        if (manager == null) return null;
        ActivityManager.MemoryInfo info = new ActivityManager.MemoryInfo();
        manager.getMemoryInfo(info);
        return info.totalMem + "," + info.availMem + "," + info.lowMemory;
    }
    private boolean execute(Runnable job) {
        try { files.execute(job); return true; }
        catch (RejectedExecutionException error) { notice("File operation failed", error); return false; }
    }

    public void pickDrawing() { pick(OPEN); }
    public void pickLinked() { pick(LINK); }
    public void pickProject() { pick(PROJECT); }
    private void pick(int request) {
        runOnUiThread(() -> {
            if (picking || isFinishing() || isDestroyed()) return;
            picking = true;
            Intent intent = new Intent(request == PROJECT ? Intent.ACTION_OPEN_DOCUMENT_TREE : Intent.ACTION_OPEN_DOCUMENT);
            if (request != PROJECT) { intent.addCategory(Intent.CATEGORY_OPENABLE); intent.setType("*/*"); }
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            if (request == LINK) intent.addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            try { startActivityForResult(intent, request); }
            catch (RuntimeException error) { picking = false; notice("Open", error); }
        });
    }
    private void openIntent(Intent intent) {
        if (intent == null || !Intent.ACTION_VIEW.equals(intent.getAction()) || intent.getData() == null) return;
        Uri uri = intent.getData();
        if (!"content".equals(uri.getScheme())) return;
        execute(() -> { try { imported(importDrawing(uri, false, 0)); } catch (Exception error) { notice("Open", error); } });
    }

    public void enterCommand(String initial, String prompt, boolean numeric) { enter(COMMAND, initial, prompt, numeric); }
    public void nameDrawing(String initial) { enter(NAME, initial, tr("Drawing name"), false); }
    private void enter(int request, String initial, String prompt, boolean numeric) {
        runOnUiThread(() -> {
            if (picking || isFinishing() || isDestroyed()) { if (request == NAME) event("name-canceled", ""); return; }
            picking = true;
            Intent intent = new Intent(this, CommandActivity.class).putExtra("initial", initial).putExtra("prompt", prompt).putExtra("numeric", numeric).putExtra("filename", request == NAME);
            try { startActivityForResult(intent, request); }
            catch (RuntimeException error) { picking = false; if (request == NAME) event("name-canceled", ""); notice("Keyboard", error); }
        });
    }

    private File privateFile(String path) throws Exception {
        File source = new File(path).getCanonicalFile();
        if (!source.getPath().startsWith(getFilesDir().getCanonicalPath() + File.separator) || !source.isFile()) throw new IllegalArgumentException(tr("The drawing has not been saved in the app"));
        return source;
    }
    public void exportDrawing(String path, String name) {
        runOnUiThread(() -> {
            if (picking || isFinishing() || isDestroyed()) return;
            picking = true;
            if (!execute(() -> {
                File snapshot = null;
                try {
                    File source = privateFile(path);
                    snapshot = File.createTempFile("cadcraft-export-", ".drawing", getCacheDir());
                    try (InputStream in = new FileInputStream(source); FileOutputStream out = new FileOutputStream(snapshot)) { copy(in, out, memoryBudget()); out.getFD().sync(); }
                    File ready = snapshot;
                    runOnUiThread(() -> {
                        if (isFinishing() || isDestroyed()) { ready.delete(); picking = false; return; }
                        exporting = ready;
                        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType(mime(name)).putExtra(Intent.EXTRA_TITLE, clean(name));
                        try { startActivityForResult(intent, EXPORT); }
                        catch (RuntimeException error) { picking = false; clearExport(); notice("Export", error); }
                    });
                } catch (Exception error) {
                    if (snapshot != null) snapshot.delete();
                    runOnUiThread(() -> { picking = false; notice("Export", error); });
                }
            })) picking = false;
        });
    }
    public void shareDrawing(String path, String name) {
        execute(() -> {
            File folder = null;
            try {
                File source = privateFile(path);
                File root = new File(getCacheDir(), "shared");
                File[] old = root.listFiles();
                if (old != null) for (File f : old) if (System.currentTimeMillis() - f.lastModified() > 86400000L) remove(f, 0);
                String id = UUID.randomUUID().toString();
                folder = new File(root, id);
                if (!folder.mkdirs()) throw new IllegalStateException(tr("Could not prepare a shared file"));
                File snapshot = new File(folder, clean(name));
                try (InputStream in = new FileInputStream(source); FileOutputStream out = new FileOutputStream(snapshot)) { copy(in, out, memoryBudget()); }
                Uri uri = new Uri.Builder().scheme("content").authority(getPackageName() + ".share").appendPath(id).appendPath(snapshot.getName()).build();
                Intent intent = new Intent(Intent.ACTION_SEND).setType(mime(name)).putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                intent.setClipData(ClipData.newRawUri(snapshot.getName(), uri));
                runOnUiThread(() -> { try { startActivity(Intent.createChooser(intent, tr("Export"))); } catch (RuntimeException error) { notice("Export", error); } });
            } catch (Exception error) { if (folder != null) remove(folder, 0); notice("Export", error); }
        });
    }

    @Override protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == COMMAND || request == NAME) {
            picking = false;
            String value = result == RESULT_OK && data != null ? data.getStringExtra("command") : null;
            if (request == NAME) event(value == null ? "name-canceled" : "name", value == null ? "" : value);
            else if (value != null) { commands.offer(value); wake(0); }
            return;
        }
        if (request != OPEN && request != LINK && request != PROJECT && request != EXPORT) return;
        if (result != RESULT_OK || data == null || data.getData() == null) { picking = false; if (request == EXPORT) clearExport(); return; }
        Uri uri = data.getData();
        int flags = data.getFlags();
        File snapshot = exporting;
        exporting = null;
        if (!execute(() -> {
            boolean choosing = false;
            try {
                if (request == OPEN || request == LINK) imported(importDrawing(uri, request == LINK, flags));
                else if (request == PROJECT) { importProject(uri); choosing = true; }
                else if (snapshot != null) {
                    try (InputStream in = new FileInputStream(snapshot); OutputStream out = getContentResolver().openOutputStream(uri, "wt")) {
                        if (out == null) throw new IllegalStateException(tr("The provider did not return a writable file"));
                        copy(in, out, memoryBudget());
                    }
                    notice(tr("Drawing exported"));
                }
            } catch (Exception error) { notice(request == EXPORT ? "Export" : "Open", error); }
            finally { if (snapshot != null) snapshot.delete(); if (!choosing) runOnUiThread(() -> picking = false); }
        })) { picking = false; if (snapshot != null) snapshot.delete(); }
    }

    private static String clean(String name) {
        if (name == null || name.isEmpty() || name.equals(".") || name.equals("..")) return "Drawing.dxf";
        name = name.replace('/', '_').replace('\\', '_').replaceAll("[\\p{Cntrl}]", "_");
        if (name.length() > 180) {
            int dot = name.lastIndexOf('.');
            String extension = dot >= 0 && name.length() - dot < 8 ? name.substring(dot) : "";
            name = name.substring(0, 180 - extension.length()) + extension;
        }
        return name;
    }
    private static String mime(String name) {
        String extension = name.substring(name.lastIndexOf('.') + 1).toLowerCase(Locale.ROOT);
        String mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension);
        return mime == null ? "application/octet-stream" : mime;
    }
    private static boolean drawing(String name) { String lower = name.toLowerCase(Locale.ROOT); return lower.endsWith(".dxf") || lower.endsWith(".dwg"); }
    private String displayName(Uri uri) {
        try (Cursor c = getContentResolver().query(uri, new String[]{OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (c != null && c.moveToFirst() && !c.isNull(0)) return clean(c.getString(0));
        } catch (RuntimeException ignored) {}
        return "Drawing.dxf";
    }
    private boolean writable(Uri uri, int flags) {
        if ((flags & Intent.FLAG_GRANT_WRITE_URI_PERMISSION) == 0) return false;
        try (Cursor c = getContentResolver().query(uri, new String[]{DocumentsContract.Document.COLUMN_FLAGS}, null, null, null)) {
            return c != null && c.moveToFirst() && (c.getInt(0) & DocumentsContract.Document.FLAG_SUPPORTS_WRITE) != 0;
        } catch (RuntimeException ignored) { return false; }
    }
    private File importDrawing(Uri uri, boolean linked, int flags) throws Exception {
        String name = displayName(uri);
        if (!drawing(name)) throw new IllegalArgumentException(tr("Choose a DXF or DWG drawing"));
        File folder = new File(new File(getFilesDir(), "imports"), UUID.randomUUID().toString());
        if (!folder.mkdirs()) throw new IllegalStateException(tr("Could not create the import folder"));
        File target = new File(folder, name);
        try (InputStream in = getContentResolver().openInputStream(uri); FileOutputStream out = new FileOutputStream(target)) {
            if (in == null) throw new IllegalStateException(tr("The provider did not return a readable file"));
            copy(in, out, memoryBudget()); out.getFD().sync();
        } catch (Exception error) { remove(folder, 0); throw error; }
        if (linked) {
            if (writable(uri, flags)) {
                try {
                    getContentResolver().takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
                    if (!getSharedPreferences("linked-drawings", MODE_PRIVATE).edit().putString(target.getCanonicalPath(), uri.toString()).commit()) throw new IllegalStateException(tr("Could not remember the linked drawing"));
                } catch (RuntimeException error) { notice(tr("Read-only provider; opened a copy")); }
            } else notice(tr("Read-only provider; opened a copy"));
        }
        return target;
    }
    public void syncLinkedDrawing(String path) {
        FileInputStream captured = null;
        try {
            File source = privateFile(path);
            String address = getSharedPreferences("linked-drawings", MODE_PRIVATE).getString(source.getCanonicalPath(), null);
            if (address == null) return;
            Uri uri = Uri.parse(address);
            // Hold the saved inode now; another atomic local save may run before this job starts.
            captured = new FileInputStream(source);
            FileInputStream input = captured;
            if (!execute(() -> {
                File snapshot = null;
                try (InputStream in = input) {
                    snapshot = File.createTempFile("cadcraft-linked-", ".drawing", getCacheDir());
                    try (FileOutputStream out = new FileOutputStream(snapshot)) { copy(in, out, memoryBudget()); out.getFD().sync(); }
                    byte[] hash = MessageDigest.getInstance("SHA-256").digest(source.getCanonicalPath().getBytes(StandardCharsets.UTF_8));
                    StringBuilder id = new StringBuilder(); for (byte b : hash) id.append(String.format(Locale.ROOT, "%02x", b & 255));
                    File backupFolder = new File(new File(getFilesDir(), "linked-backups"), id.toString());
                    if (!backupFolder.isDirectory() && !backupFolder.mkdirs()) throw new IllegalStateException(tr("Could not create a backup folder"));
                    File backup = new File(backupFolder, source.getName()), temp = new File(backupFolder, source.getName() + ".tmp");
                    try (InputStream old = getContentResolver().openInputStream(uri); FileOutputStream out = new FileOutputStream(temp)) {
                        if (old == null) throw new IllegalStateException(tr("Could not read the original for backup"));
                        copy(old, out, memoryBudget()); out.getFD().sync();
                    }
                    if (!temp.renameTo(backup)) throw new IllegalStateException(tr("Could not preserve the original backup"));
                    try (InputStream saved = new FileInputStream(snapshot); OutputStream out = getContentResolver().openOutputStream(uri, "wt")) {
                        if (out == null) throw new IllegalStateException(tr("The provider did not return a writable file"));
                        copy(saved, out, memoryBudget());
                    }
                    notice(tr("Linked drawing saved"));
                } catch (Exception error) { notice(tr("External save failed; local copy and backup kept") + ": " + error.getMessage()); }
                finally { if (snapshot != null) snapshot.delete(); }
            })) input.close();
        } catch (Exception error) { if (captured != null) try { captured.close(); } catch (Exception ignored) {} notice("Save", error); }
    }

    private void importProject(Uri tree) throws Exception {
        File folder = new File(new File(getFilesDir(), "imports"), UUID.randomUUID().toString());
        if (!folder.mkdirs()) throw new IllegalStateException(tr("Could not create the project folder"));
        ArrayList<File> drawings = new ArrayList<>();
        long[] budget = {memoryBudget(), 0};
        try { copyTree(tree, DocumentsContract.getTreeDocumentId(tree), folder, 0, budget, drawings); }
        catch (Exception error) { remove(folder, 0); throw error; }
        if (drawings.isEmpty()) { remove(folder, 0); throw new IllegalArgumentException(tr("Choose a folder containing DXF or DWG drawings")); }
        String[] names = new String[drawings.size()];
        for (int i = 0; i < names.length; i++) names[i] = folder.toURI().relativize(drawings.get(i).toURI()).getPath();
        runOnUiThread(() -> {
            if (isFinishing() || isDestroyed()) { picking = false; return; }
            new AlertDialog.Builder(this).setTitle(tr("Choose drawing")).setItems(names, (dialog, index) -> {
                picking = false; if (index >= 0 && index < drawings.size()) imported(drawings.get(index));
            }).setNegativeButton(tr("Cancel"), (dialog, which) -> picking = false).setOnCancelListener(dialog -> picking = false).show();
        });
    }
    private void copyTree(Uri tree, String id, File folder, int depth, long[] budget, ArrayList<File> drawings) throws Exception {
        if (depth > 8) throw new IllegalArgumentException(tr("Project folders are nested too deeply"));
        Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id);
        try (Cursor c = getContentResolver().query(children, new String[]{DocumentsContract.Document.COLUMN_DOCUMENT_ID, DocumentsContract.Document.COLUMN_DISPLAY_NAME, DocumentsContract.Document.COLUMN_MIME_TYPE}, null, null, null)) {
            if (c == null) throw new IllegalStateException(tr("Could not read the project folder"));
            while (c.moveToNext()) {
                if (++budget[1] > 128) throw new IllegalArgumentException(tr("Project contains too many files or folders"));
                String name = clean(c.getString(1)), kind = c.getString(2);
                File target = new File(folder, name);
                if (DocumentsContract.Document.MIME_TYPE_DIR.equals(kind)) {
                    if (!target.mkdirs()) throw new IllegalStateException(tr("Could not create a project subfolder"));
                    copyTree(tree, c.getString(0), target, depth + 1, budget, drawings);
                } else {
                    String lower = name.toLowerCase(Locale.ROOT);
                    if (!drawing(name) && !lower.endsWith(".png") && !lower.endsWith(".jpg") && !lower.endsWith(".jpeg")) continue;
                    if (target.exists()) throw new IllegalArgumentException(tr("Project contains duplicate file names"));
                    Uri uri = DocumentsContract.buildDocumentUriUsingTree(tree, c.getString(0));
                    try (InputStream in = getContentResolver().openInputStream(uri); FileOutputStream out = new FileOutputStream(target)) {
                        if (in == null) throw new IllegalStateException(tr("Could not read a project file"));
                        budget[0] -= copy(in, out, budget[0]); out.getFD().sync();
                    }
                    if (drawing(name)) drawings.add(target);
                }
            }
        }
    }
    private static long copy(InputStream in, OutputStream out, long limit) throws Exception {
        byte[] buffer = new byte[65536]; long total = 0;
        for (int n; (n = in.read(buffer)) != -1;) {
            total += n;
            if (total > limit) throw new IllegalArgumentException(tr("This file exceeds the available memory budget"));
            out.write(buffer, 0, n);
        }
        return total;
    }
    private static void remove(File file, int depth) {
        if (depth > 10) return;
        File[] children = file.listFiles(); if (children != null) for (File child : children) remove(child, depth + 1);
        file.delete();
    }
    private void clearExport() { if (exporting != null) { exporting.delete(); exporting = null; } }
    @Override protected void onDestroy() { if (current.get() == this) current.clear(); files.shutdown(); clearExport(); super.onDestroy(); }

    /** A separate Java input queue gives commands composing IME and numeric keyboards. */
    public static final class CommandActivity extends Activity {
        private EditText input;
        @Override protected void onCreate(Bundle state) {
            super.onCreate(state);
            int padding = (int) (20 * getResources().getDisplayMetrics().density), height = (int) (56 * getResources().getDisplayMetrics().density);
            boolean numeric = getIntent().getBooleanExtra("numeric", false), filename = getIntent().getBooleanExtra("filename", false);
            LinearLayout panel = new LinearLayout(this); panel.setOrientation(LinearLayout.VERTICAL); panel.setPadding(padding, padding, padding, padding);
            TextView title = new TextView(this); title.setText(getIntent().getStringExtra("prompt")); title.setTextSize(20); panel.addView(title);
            input = new EditText(this); input.setId(android.R.id.edit); input.setSingleLine(true);
            input.setInputType(numeric ? InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_DECIMAL | InputType.TYPE_NUMBER_FLAG_SIGNED : InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
            input.setImeOptions(EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_EXTRACT_UI);
            input.setFilters(new InputFilter[]{new InputFilter.LengthFilter(filename ? 180 : 8192)});
            input.setText(getIntent().getStringExtra("initial")); input.setSelection(input.length());
            panel.addView(input, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, height));
            if (!numeric && !filename) {
                LinearLayout symbols = new LinearLayout(this);
                for (String symbol : new String[]{"@", ",", "<", "-", "."}) {
                    Button button = new Button(this); button.setText(symbol); button.setOnClickListener(view -> {
                        int start = Math.max(0, input.getSelectionStart()), end = Math.max(start, input.getSelectionEnd()); input.getText().replace(start, end, symbol);
                    }); symbols.addView(button, new LinearLayout.LayoutParams(0, height, 1));
                }
                panel.addView(symbols);
            }
            LinearLayout buttons = new LinearLayout(this);
            Button cancel = new Button(this); cancel.setText(tr("Cancel")); cancel.setOnClickListener(view -> finish());
            Button enter = new Button(this); enter.setText(tr(filename ? "Save" : "Enter")); enter.setOnClickListener(view -> submit());
            buttons.addView(cancel, new LinearLayout.LayoutParams(0, height, 1)); buttons.addView(enter, new LinearLayout.LayoutParams(0, height, 1)); panel.addView(buttons);
            setContentView(panel);
            getWindow().setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_VISIBLE | WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
            input.setOnEditorActionListener((view, action, event) -> {
                if (action != EditorInfo.IME_ACTION_DONE && !(event != null && event.getKeyCode() == KeyEvent.KEYCODE_ENTER && event.getAction() == KeyEvent.ACTION_DOWN)) return false;
                submit(); return true;
            });
            input.requestFocus();
        }
        @Override public void onWindowFocusChanged(boolean focused) { super.onWindowFocusChanged(focused); if (focused && input != null) {
            InputMethodManager ime = getSystemService(InputMethodManager.class); if (ime != null) ime.showSoftInput(input, InputMethodManager.SHOW_IMPLICIT);
        } }
        private void submit() { setResult(RESULT_OK, new Intent().putExtra("command", input.getText().toString())); finish(); }
    }
}
