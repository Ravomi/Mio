/**
 * Mio Official Site - Main Interaction Script
 * Features:
 * - Code / Event tab toggling with WAI-ARIA tabs pattern (roving tabindex, arrow/Home/End keys)
 * - Copy to clipboard with success/failure feedback + aria-live announcements
 * - Graceful degradation if JavaScript is disabled (tabs default to visible, JS adds .js-enabled)
 */

(function () {
  'use strict';

  // Mark document body as JS enabled so progressive enhancement styles apply
  document.documentElement.classList.add('js-enabled');

  function announceStatus(message) {
    var statusEl = document.getElementById('copy-status-live');
    if (statusEl) {
      statusEl.textContent = '';
      // Trigger live region readout
      setTimeout(function () {
        statusEl.textContent = message;
      }, 50);
    }
  }

  // --- Copy to clipboard support ---
  function setupCopyButtons() {
    var copyButtons = document.querySelectorAll('.copy-btn');
    copyButtons.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var targetId = btn.getAttribute('data-target');
        var targetEl = document.getElementById(targetId);
        if (!targetEl) return;

        var textToCopy = targetEl.innerText || targetEl.textContent;

        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(textToCopy).then(
            function () {
              markCopySuccess(btn);
            },
            function () {
              fallbackCopy(textToCopy, btn);
            }
          );
        } else {
          fallbackCopy(textToCopy, btn);
        }
      });
    });
  }

  function fallbackCopy(text, btn) {
    try {
      var textarea = document.createElement('textarea');
      textarea.value = text;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      textarea.focus();
      textarea.select();
      var successful = document.execCommand('copy');
      document.body.removeChild(textarea);
      if (successful) {
        markCopySuccess(btn);
      } else {
        markCopyFailure(btn);
      }
    } catch (err) {
      markCopyFailure(btn);
    }
  }

  function markCopySuccess(btn) {
    var originalText = btn.getAttribute('data-label') || '复制';
    btn.textContent = '已复制 ✓';
    btn.classList.add('copied');
    announceStatus('已复制命令到剪贴板');
    setTimeout(function () {
      btn.textContent = originalText;
      btn.classList.remove('copied');
    }, 2000);
  }

  function markCopyFailure(btn) {
    var originalText = btn.getAttribute('data-label') || '复制';
    btn.textContent = '复制失败';
    announceStatus('复制到剪贴板失败，请手动选择复制');
    setTimeout(function () {
      btn.textContent = originalText;
    }, 2000);
  }

  // --- Hero panel tab switcher with WAI-ARIA tabs keyboard navigation ---
  function setupPanelTabs() {
    var tablist = document.querySelector('.panel-tab-group[role="tablist"]');
    var tabButtons = Array.prototype.slice.call(document.querySelectorAll('.panel-tab-btn[role="tab"]'));
    var tabPanels = Array.prototype.slice.call(document.querySelectorAll('.tab-panel-content[role="tabpanel"]'));

    if (!tablist || !tabButtons.length || !tabPanels.length) return;

    function activateTab(targetBtn, focus) {
      var targetTabId = targetBtn.getAttribute('aria-controls') || targetBtn.getAttribute('data-tab');

      tabButtons.forEach(function (btn) {
        var isTarget = btn === targetBtn;
        btn.classList.toggle('active', isTarget);
        btn.setAttribute('aria-selected', isTarget ? 'true' : 'false');
        btn.setAttribute('tabindex', isTarget ? '0' : '-1');
      });

      tabPanels.forEach(function (panel) {
        var isTargetPanel = panel.id === targetTabId;
        panel.style.display = isTargetPanel ? 'block' : 'none';
        panel.setAttribute('aria-hidden', isTargetPanel ? 'false' : 'true');
      });

      if (focus) {
        targetBtn.focus();
      }
    }

    // Initialize initial state based on active button
    var initialActiveBtn = tabButtons.find(function (btn) {
      return btn.classList.contains('active') || btn.getAttribute('aria-selected') === 'true';
    }) || tabButtons[0];
    activateTab(initialActiveBtn, false);

    tabButtons.forEach(function (btn) {
      btn.addEventListener('click', function () {
        activateTab(btn, false);
      });
    });

    // Keyboard navigation: Left/Right arrow, Home, End
    tablist.addEventListener('keydown', function (e) {
      var currentIndex = tabButtons.indexOf(document.activeElement);
      if (currentIndex === -1) return;

      var targetIndex = -1;
      switch (e.key) {
        case 'ArrowLeft':
        case 'ArrowUp':
          targetIndex = (currentIndex - 1 + tabButtons.length) % tabButtons.length;
          break;
        case 'ArrowRight':
        case 'ArrowDown':
          targetIndex = (currentIndex + 1) % tabButtons.length;
          break;
        case 'Home':
          targetIndex = 0;
          break;
        case 'End':
          targetIndex = tabButtons.length - 1;
          break;
        default:
          return;
      }

      e.preventDefault();
      activateTab(tabButtons[targetIndex], true);
    });
  }

  // Initialize once DOM is ready
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
      setupCopyButtons();
      setupPanelTabs();
    });
  } else {
    setupCopyButtons();
    setupPanelTabs();
  }
})();
