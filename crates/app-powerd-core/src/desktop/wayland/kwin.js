// KWin-side focus bridge for app-powerd's KDE Plasma backend.
//
// This script is bundled into the Rust binary, written to the user's runtime
// directory, and loaded through org.kde.kwin.Scripting. It watches KWin window
// activation, closure, and active-window fullscreen changes. Each observation
// is serialized as JSON and sent to app-powerd's session D-Bus Event method.
// KWin owns the window objects; this bridge only reads their metadata.

const bridgeService = "io.github.johnlepikhin.AppPowerd.KWin";
const bridgePath = "/io/github/johnlepikhin/AppPowerd/KWin";
const bridgeInterface = "io.github.johnlepikhin.AppPowerd.KWin";

function windowData(window) {
    if (!window) {
        return null;
    }

    return {
        id: String(window.internalId),
        pid: window.pid,
        title: window.caption,
        appId: window.desktopFileName,
        resourceClass: window.resourceClass,
        fullscreen: window.fullScreen
    };
}

function emit(kind, window) {
    callDBus(
        bridgeService,
        bridgePath,
        bridgeInterface,
        "Event",
        JSON.stringify({ kind: kind, window: windowData(window) })
    );
}

function watch(window) {
    if (!window) {
        return;
    }

    window.fullScreenChanged.connect(function () {
        if (window.active) {
            emit("focused", window);
        }
    });
}

const plasma6 = workspace.windowAdded !== undefined;
const windows = plasma6 ? workspace.stackingOrder : workspace.clientList();
const windowAdded = plasma6 ? workspace.windowAdded : workspace.clientAdded;
const windowActivated = plasma6 ? workspace.windowActivated : workspace.clientActivated;
const windowRemoved = plasma6 ? workspace.windowRemoved : workspace.clientRemoved;
const activeWindow = plasma6 ? workspace.activeWindow : workspace.activeClient;

windows.forEach(watch);
windowAdded.connect(watch);
windowActivated.connect(function (window) {
    emit("focused", window);
});
windowRemoved.connect(function (window) {
    emit("closed", window);
});

emit("focused", activeWindow);