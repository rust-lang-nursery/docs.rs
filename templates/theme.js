function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
}

window.addEventListener('storage', function (ev) {
  if (ev.key === 'rustdoc-theme') {
    applyTheme(ev.newValue);
  }
});

// see ./storage-change-detection.html for details
window.addEventListener('message', function (ev) {
  if (ev.data && ev.data.storage && ev.data.storage.key === 'rustdoc-theme') {
    applyTheme(ev.data.storage.value);
  }
});

applyTheme(window.localStorage.getItem('rustdoc-theme'));
