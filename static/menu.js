// Allow menus to be open and used by keyboard.
(function() {
    var currentMenu;
    var backdrop = document.createElement("div");
    backdrop.style = "display:none;position:fixed;width:100%;height:100%;z-index:1";
    document.documentElement.insertBefore(backdrop, document.querySelector("body"));
    function previous(allItems, item) {
        var i = 1;
        var l = allItems.length;
        while (i < l) {
            if (allItems[i] == item) {
                return allItems[i - 1];
            }
            i += 1;
        }
    }
    function next(allItems, item) {
        var i = 0;
        var l = allItems.length - 1;
        while (i < l) {
            if (allItems[i] == item) {
                return allItems[i + 1];
            }
            i += 1;
        }
    }
    function last(allItems) {
        return allItems[allItems.length - 1];
    }
    function closeMenu() {
        if (this === backdrop) {
            var rustdoc = document.querySelector(".rustdoc");
            if (rustdoc) {
                rustdoc.focus();
            } else {
                document.documentElement.focus();
            }
        } else if (currentMenu.querySelector(".pure-menu-link:focus")) {
            currentMenu.firstElementChild.focus();
        }
        currentMenu.className = currentMenu.className.replace("pure-menu-active", "");
        currentMenu = null;
        backdrop.style.display = "none";
    }
    backdrop.onclick = closeMenu;
    function openMenu(newMenu) {
        currentMenu = newMenu;
        newMenu.className += " pure-menu-active";
        backdrop.style.display = "block";
    }
    function menuOnClick(e) {
        // The "About", "Releases", and "Rust" menus act as menus on desktop,
        // and links on mobile (due to pure-menu-opt-children). On desktop,
        // we inhibit navigation events on click so the menu open action can
        // take effect. Detect desktop by noticing that the second child
        // is hidden.
        console.log(this.children[1]);
        console.log(window.getComputedStyle(this.children[1]).display);
        if (this.children.length > 1 && window.getComputedStyle(this.children[1]).display != 'none') {
            return false;
        }
        if (this.getAttribute("href") != "#") {
            return;
        }
        if (this.parentNode === currentMenu) {
            closeMenu();
            this.blur();
        } else {
            if (currentMenu) closeMenu();
            openMenu(this.parentNode);
        }
        e.preventDefault();
        e.stopPropagation();
    };
    function menuKeyDown(e) {
        if (currentMenu) {
            var children = currentMenu.querySelector(".pure-menu-children");
            var currentLink = children.querySelector(".pure-menu-link:focus");
            var currentItem;
            if (currentLink && currentLink.parentNode.className.indexOf("pure-menu-item") !== -1) {
                currentItem = currentLink.parentNode;
            }
            var allItems = [];
            if (children) {
                allItems = children.querySelectorAll(".pure-menu-item .pure-menu-link");
            }
            var switchTo = null;
            switch (e.key.toLowerCase()) {
                case "escape":
                case "esc":
                    closeMenu();
                    e.preventDefault();
                    e.stopPropagation();
                    return;
                case "arrowdown":
                case "down":
                    if (currentLink) {
                        // Arrow down when an item other than the last is focused: focus next item.
                        // Arrow down when the last item is focused: jump to top.
                        switchTo = (next(allItems, currentLink) || allItems[0]);
                    } else {
                        // Arrow down when a menu is open and nothing is focused: focus first item.
                        switchTo = allItems[0];
                    }
                    break;
                case "arrowup":
                case "up":
                    if (currentLink) {
                        // Arrow up when an item other than the first is focused: focus previous item.
                        // Arrow up when the first item is focused: jump to bottom.
                        switchTo = (previous(allItems, currentLink) || last(allItems));
                    } else {
                        // Arrow up when a menu is open and nothing is focused: focus last item.
                        switchTo = last(allItems);
                    }
                    break;
                case "tab":
                    if (!currentLink) {
                        // if the menu is open, we should focus trap into it
                        // this is the behavior of the WAI example
                        // it is not the same as GitHub, but GitHub allows you to tab yourself out
                        // of the menu without closing it (which is horrible behavior)
                        switchTo = e.shiftKey ? last(allItems) : allItems[0];
                    } else if (e.shiftKey && currentLink === allItems[0]) {
                        // if you tab your way out of the menu, close it
                        // this is neither what GitHub nor the WAI example do,
                        // but is a rationalization of GitHub's behavior: we don't want users who know how to
                        // use tab and enter, but don't know that they can close menus with Escape,
                        // to find themselves completely trapped in the menu
                        closeMenu();
                        e.preventDefault();
                        e.stopPropagation();
                    } else if (!e.shiftKey && currentLink === last(allItems)) {
                        // same as above.
                        // if you tab your way out of the menu, close it
                        closeMenu();
                    }
                    break;
                case "enter":
                case "return":
                case "space":
                case " ":
                    // enter, return, and space have the default browser behavior,
                    // but they also close the menu
                    // this behavior is identical between both the WAI example, and GitHub's
                    setTimeout(function() {
                        closeMenu();
                    }, 100);
                    break;
                case "home":
                case "pageup":
                    // home: focus first menu item.
                    // This is the behavior of WAI, while GitHub scrolls,
                    // but it's unlikely that a user will try to scroll the page while the menu is open,
                    // so they won't do it on accident.
                    switchTo = allItems[0];
                    break;
                case "end":
                case "pagedown":
                    // end: focus last menu item.
                    // This is the behavior of WAI, while GitHub scrolls,
                    // but it's unlikely that a user will try to scroll the page while the menu is open,
                    // so they won't do it on accident.
                    switchTo = last(allItems);
                    break;
            }
            if (switchTo) {
                var switchToLink = switchTo.querySelector("a");
                if (switchToLink) {
                    switchToLink.focus();
                } else {
                    switchTo.focus();
                }
                e.preventDefault();
                e.stopPropagation();
            }
        } else if (e.target.parentNode.className && e.target.parentNode.className.indexOf("pure-menu-has-children") !== -1) {
            switch (e.key.toLowerCase()) {
                case "arrowdown":
                case "down":
                case "space":
                case " ":
                    openMenu(e.target.parentNode);
                    e.preventDefault();
                    e.stopPropagation();
                    break;
            }
        }
    };
    var menus = Array.prototype.slice.call(document.querySelectorAll(".pure-menu-has-children"));
    var menusLength = menus.length;
    var menu;
    for (var i = 0; i < menusLength; ++i) {
        menu = menus[i];
        menu.firstElementChild.setAttribute("aria-haspopup", "menu");
        menu.firstElementChild.nextElementSibling.setAttribute("role", "menu");
        menu.firstElementChild.addEventListener("click", menuOnClick);
    }
    document.documentElement.addEventListener("keydown", menuKeyDown);
})();
