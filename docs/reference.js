/**
 * Mio Lightweight Reference Engine (docs/reference.js)
 * Clean, lightweight, zero external dependencies.
 * Shared across documentation.html, models.html, changelog.html.
 * Handles:
 * - Theme Engine: syncs with mio:theme (light/dark/auto)
 * - Language Engine: toggle zh/en, updates [data-zh]/[data-en]
 * - Code Copy: clipboard copy with accessible live region
 * - Mobile Navigation Drawer
 * - Anchor Link Highlight & Sticky Sidebar Scroll Spy
 * - Catalog filtering & search (for models.html)
 * - Search in sidebar (for documentation.html)
 */

(function () {
  'use strict';

  var currentLang = 'zh';

  // 1. Theme Engine
  function initTheme() {
    var btn = document.getElementById('themeToggleBtn');
    var iconEl = document.getElementById('themeIcon');
    var labelEl = document.getElementById('themeLabel');
    var media = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;

    function resolveTheme(mode) {
      if (mode === 'auto') {
        return media && media.matches ? 'dark' : 'light';
      }
      return mode === 'dark' ? 'dark' : 'light';
    }

    function applyTheme(mode, persist) {
      var resolved = resolveTheme(mode);
      document.documentElement.dataset.theme = resolved;
      document.documentElement.dataset.themeMode = mode;
      if (persist) {
        try {
          localStorage.setItem('mio:theme', mode);
        } catch (e) {}
      }
      if (iconEl) {
        iconEl.textContent = mode === 'dark' ? '' : mode === 'auto' ? '◐' : '';
      }
      if (labelEl) {
        var labels = {
          light: { zh: '浅色', en: 'Light' },
          dark: { zh: '深色', en: 'Dark' },
          auto: { zh: '自动', en: 'Auto' }
        };
        var item = labels[mode] || labels.light;
        labelEl.dataset.zh = item.zh;
        labelEl.dataset.en = item.en;
        labelEl.textContent = currentLang === 'en' ? item.en : item.zh;
      }
    }

    var savedMode = 'dark';
    try {
      savedMode = localStorage.getItem('mio:theme') || 'dark';
    } catch (e) {}

    applyTheme(savedMode, false);

    if (btn) {
      btn.addEventListener('click', function () {
        var cur = document.documentElement.dataset.themeMode || 'dark';
        var next = cur === 'light' ? 'dark' : cur === 'dark' ? 'auto' : 'light';
        applyTheme(next, true);
      });
    }

    if (media && media.addEventListener) {
      media.addEventListener('change', function () {
        if ((document.documentElement.dataset.themeMode || 'dark') === 'auto') {
          applyTheme('auto', false);
        }
      });
    }
  }

  // 2. Language Engine
  function initLanguage() {
    var btn = document.getElementById('langToggleBtn');

    function applyLanguage(lang) {
      currentLang = lang;
      document.documentElement.lang = lang === 'en' ? 'en' : 'zh-CN';
      if (btn) {
        btn.textContent = lang === 'en' ? '中文' : 'EN';
      }

      document.querySelectorAll('[data-zh][data-en]').forEach(function (el) {
        el.textContent = lang === 'en' ? el.dataset.en : el.dataset.zh;
      });

      document.querySelectorAll('[data-zh-html][data-en-html]').forEach(function (el) {
        el.innerHTML = lang === 'en' ? el.dataset.enHtml : el.dataset.zhHtml;
      });

      document.querySelectorAll('[data-zh-placeholder][data-en-placeholder]').forEach(function (el) {
        el.placeholder = lang === 'en' ? el.dataset.enPlaceholder : el.dataset.zhPlaceholder;
      });

      // Update theme label text if present
      var themeLabel = document.getElementById('themeLabel');
      if (themeLabel) {
        themeLabel.textContent = lang === 'en' ? (themeLabel.dataset.en || 'Theme') : (themeLabel.dataset.zh || '主题');
      }

    }

    if (btn) {
      btn.addEventListener('click', function () {
        applyLanguage(currentLang === 'zh' ? 'en' : 'zh');
      });
    }
  }

  // 3. Mobile Navigation Drawer
  function initMobileNav() {
    var btn = document.getElementById('mobileMenuBtn');
    var drawer = document.getElementById('mobileNavDrawer');
    if (!btn || !drawer) return;

    btn.addEventListener('click', function () {
      var isOpen = drawer.classList.toggle('is-open');
      btn.setAttribute('aria-expanded', isOpen ? 'true' : 'false');
    });

    drawer.querySelectorAll('a').forEach(function (a) {
      a.addEventListener('click', function () {
        drawer.classList.remove('is-open');
        btn.setAttribute('aria-expanded', 'false');
      });
    });
  }

  // 4. Accessible Copy Code Button

  function initCopyButtons() {
    var live = document.getElementById('copy-status-live');
    if (!live) {live=document.createElement('div');live.id='copy-status-live';live.className='sr-only';live.setAttribute('aria-live','polite');document.body.appendChild(live);}
    function fallback(text) {
      var el=document.createElement('textarea');el.value=text;el.style.position='fixed';el.style.left='-9999px';document.body.appendChild(el);el.select();
      var ok=false;try {ok=document.execCommand('copy');} finally {el.remove();}
      if(!ok) throw new Error('Clipboard unavailable');
    }
    document.querySelectorAll('.copy-code-btn,.catalog-copy-btn').forEach(function(btn) {
      btn.addEventListener('click',async function() {
        var block=btn.closest('.code-block'),code=block&&block.querySelector('pre code');
        var text=btn.dataset.copy||(code&&code.textContent);if(!text)return;
        try {
          try {if(!navigator.clipboard)throw new Error('unavailable');await navigator.clipboard.writeText(text.trim());}
          catch(e){fallback(text.trim());}
          btn.textContent=currentLang==='en'?'Copied ':'已复制 ';live.textContent=btn.textContent;
        } catch(e) {btn.textContent=currentLang==='en'?'Copy failed; select the code':'复制失败，请选择代码';live.textContent=btn.textContent;}
        setTimeout(function(){btn.textContent=currentLang==='en'?'Copy':'复制';},2000);
      });
    });
  }

  // 5. Sidebar Navigation Search & ScrollSpy
  function initDocSidebar() {
    var searchInput = document.getElementById('sidebarSearchInput');
    var navItems = document.querySelectorAll('.ref-nav-item');

    if (searchInput) {
      searchInput.addEventListener('input', function () {
        var query = searchInput.value.trim().toLowerCase();
        navItems.forEach(function (item) {
          var link = item.querySelector('a');
          if (!link) return;
          var text = link.textContent.toLowerCase();
          var href = link.getAttribute('href') || '';
          if (!query || text.indexOf(query) !== -1 || href.indexOf(query) !== -1) {
            item.style.display = '';
          } else {
            item.style.display = 'none';
          }
        });
      });
    }

    // ScrollSpy for sections
    var sections = document.querySelectorAll('section[id], article[id]');
    if (sections.length && window.IntersectionObserver) {
      var links = document.querySelectorAll('.ref-nav-item a');
      var observer = new IntersectionObserver(function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            var id = entry.target.id;
            links.forEach(function (l) {
              if (l.getAttribute('href') === '#' + id) {
                l.classList.add('is-active');
              } else {
                l.classList.remove('is-active');
              }
            });
          }
        });
      }, { rootMargin: '-20% 0px -70% 0px' });

      sections.forEach(function (sec) {
        observer.observe(sec);
      });
    }
  }

  function initReferenceControls() {
    var sideBtn=document.getElementById('sidebarToggleBtn');
    var side=document.querySelector('.ref-sidebar-card');
    if(sideBtn&&side) {
      side.id=side.id||'sidebarContents';sideBtn.setAttribute('aria-controls',side.id);
      sideBtn.addEventListener('click',function(){var open=side.classList.toggle('is-open');sideBtn.setAttribute('aria-expanded',String(open));});
      side.querySelectorAll('a').forEach(function(a){a.addEventListener('click',function(){side.classList.remove('is-open');sideBtn.setAttribute('aria-expanded','false');});});
    }
    document.addEventListener('keydown',function(e){if(e.key!=='Escape')return;
      var nav=document.getElementById('mobileNavDrawer'),btn=document.getElementById('mobileMenuBtn');
      if(nav)nav.classList.remove('is-open');if(btn)btn.setAttribute('aria-expanded','false');
      if(side)side.classList.remove('is-open');if(sideBtn)sideBtn.setAttribute('aria-expanded','false');
    });
    var query=document.getElementById('cliFlagsSearchInput');
    if(query) {
      var section=document.getElementById('cli-flags'),rows=section.querySelectorAll('tbody tr');
      var empty=document.createElement('p');empty.id='cliFlagsEmpty';empty.hidden=true;empty.setAttribute('aria-live','polite');section.appendChild(empty);
      function filterFlags(){var q=query.value.trim().toLowerCase(),count=0;rows.forEach(function(row){var match=!q||row.textContent.toLowerCase().includes(q);row.hidden=!match;if(match)count++;});empty.textContent=currentLang==='en'?'No matching parameters':'未找到匹配参数';empty.hidden=count!==0;}
      query.addEventListener('input',filterFlags);
      new MutationObserver(filterFlags).observe(document.documentElement,{attributes:true,attributeFilter:['lang']});
    }
    var cat=document.getElementById('changelogCategorySelect');
    if(cat) {
      var timeline=document.querySelector('.timeline');
      var empty=document.createElement('p');empty.id='changelogEmpty';empty.hidden=true;empty.setAttribute('aria-live','polite');timeline.after(empty);
      function filterChanges(){var category=cat.value,count=0;document.querySelectorAll('.timeline-entry').forEach(function(entry){
        var matches=0;entry.querySelectorAll('.changelog-group').forEach(function(group){var show=!category||category==='all'||group.dataset.groupType===category;group.hidden=!show;group.style.display=show?'':'none';if(show)matches++;});
        entry.hidden=matches===0;if(matches)count++;
      });empty.textContent=currentLang==='en'?'No updates in this category':'此分类暂无更新';empty.hidden=count!==0;}
      cat.addEventListener('change',filterChanges);filterChanges();
      new MutationObserver(filterChanges).observe(document.documentElement,{attributes:true,attributeFilter:['lang']});
    }
  }

  // Initialize all on DOM ready
  document.addEventListener('DOMContentLoaded', function () {
    initTheme();
    initLanguage();
    initMobileNav();
    initCopyButtons();
    initDocSidebar();
    initReferenceControls();
  });
})();
