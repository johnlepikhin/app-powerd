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

workspace.stackingOrder.forEach(watch);
workspace.windowAdded.connect(watch);
workspace.windowActivated.connect(function (window) {
    emit("focused", window);
});
workspace.windowRemoved.connect(function (window) {
    emit("closed", window);
});

emit("focused", workspace.activeWindow);