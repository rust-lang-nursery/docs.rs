function formatCrateName(crateTitleAndVersion) {
    const stringParts = crateTitleAndVersion.split(" ", 2);
    return stringParts[0] + ' = "' + stringParts[1] + '"';
}

(function() {
    const clipboard = document.getElementById("clipboard");
    let resetClipboardTimeout = null;
    let resetClipboardIcon = clipboard.innerHTML;

    function resetClipboard() {
        resetClipboardTimeout = null;
        clipboard.innerHTML = resetClipboardIcon;
    }

    function copyTextHandler() {
        const crateTitleAndVersion = document.getElementById("crate-title");
        // On rustdoc pages, we use `textTransform: uppercase`, which copies as uppercase.
        // To avoid that, reset the styles temporarily.
        const oldTransform = crateTitleAndVersion.style.textTransform;
        crateTitleAndVersion.style.textTransform = "none";
        const temporaryInput = document.createElement("input");

        temporaryInput.type = "text";
        temporaryInput.value = formatCrateName(crateTitleAndVersion.innerText);

        document.body.append(temporaryInput);
        temporaryInput.select();
        document.execCommand("copy");

        temporaryInput.remove();
        crateTitleAndVersion.style.textTransform = oldTransform;

        clipboard.textContent = "✓";
        if (resetClipboardTimeout !== null) {
            clearTimeout(resetClipboardTimeout);
        }
        resetClipboardTimeout = setTimeout(resetClipboard, 1000);
    }

    if (clipboard != null) clipboard.addEventListener("click", copyTextHandler);
    for (const e of document.querySelectorAll('a[data-fragment="retain"]')) {
        e.addEventListener('mouseover', () => e.hash = document.location.hash);
    }
})();
