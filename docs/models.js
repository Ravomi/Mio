(function () {
  'use strict';

  var catalogData = null;
  var filterQuery = '';
  var filterProvider = 'all';

  var queryInput = document.getElementById('modelQuery');
  var providerSelect = document.getElementById('modelProvider');
  var resetButton = document.getElementById('modelReset');
  var countDisplay = document.getElementById('modelCount');
  var tbody = document.getElementById('modelRows');

  function isZh() {
    return (document.documentElement.lang || '').toLowerCase().startsWith('zh');
  }

  function getI18n(zhText, enText) {
    return isZh() ? zhText : enText;
  }

  function copyToClipboard(text, triggerBtn) {
    function indicate(ok) {
      if (!triggerBtn) return;
      var original = triggerBtn.textContent;
      triggerBtn.textContent = ok ? (isZh() ? '已复制' : 'Copied') : (isZh() ? '复制失败' : 'Failed');
      triggerBtn.disabled = true;
      setTimeout(function () {
        triggerBtn.textContent = original;
        triggerBtn.disabled = false;
      }, 1500);
    }

    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () {
        indicate(true);
      }).catch(function () {
        fallbackCopy(text, indicate);
      });
    } else {
      fallbackCopy(text, indicate);
    }
  }

  function fallbackCopy(text, indicate) {
    try {
      var textarea = document.createElement('textarea');
      textarea.value = text;
      textarea.style.position = 'fixed';
      textarea.style.top = '-9999px';
      textarea.style.left = '-9999px';
      document.body.appendChild(textarea);
      textarea.focus();
      textarea.select();
      var ok = document.execCommand('copy');
      document.body.removeChild(textarea);
      indicate(ok);
    } catch (e) {
      indicate(false);
    }
  }

  function populateProviderOptions(providers) {
    if (!providerSelect) return;
    var current = providerSelect.value;
    providerSelect.innerHTML = '';

    var optAll = document.createElement('option');
    optAll.value = 'all';
    optAll.textContent = isZh() ? '全部提供商' : 'All Providers';
    providerSelect.appendChild(optAll);

    providers.forEach(function (p) {
      var opt = document.createElement('option');
      opt.value = p.name;
      opt.textContent = p.displayName + ' (' + p.name + ')';
      providerSelect.appendChild(opt);
    });

    providerSelect.value = current || 'all';
  }

  function render() {
    if (!tbody || !countDisplay) return;

    if (!catalogData) {
      return;
    }

    var providers = catalogData.providers || [];
    var flatRows = [];

    providers.forEach(function (p) {
      if (filterProvider !== 'all' && p.name !== filterProvider) {
        return;
      }
      var models = p.models || [];
      models.forEach(function (m) {
        var q = filterQuery.toLowerCase().trim();
        var matchQ = true;
        if (q) {
          var idMatch = m.id.toLowerCase().indexOf(q) !== -1;
          var pNameMatch = p.name.toLowerCase().indexOf(q) !== -1;
          var pDisplayMatch = p.displayName.toLowerCase().indexOf(q) !== -1;
          var envMatch = (p.envVars || []).some(function (v) { return v.toLowerCase().indexOf(q) !== -1; });
          matchQ = idMatch || pNameMatch || pDisplayMatch || envMatch;
        }
        if (matchQ) {
          flatRows.push({
            provider: p,
            model: m
          });
        }
      });
    });

    tbody.innerHTML = '';

    if (flatRows.length === 0) {
      var emptyTr = document.createElement('tr');
      var emptyTd = document.createElement('td');
      emptyTd.colSpan = 5;
      emptyTd.style.textAlign = 'center';
      emptyTd.style.padding = '1.5rem';
      emptyTd.style.color = 'var(--ref-text-muted, #737373)';
      emptyTd.textContent = isZh() ? '未找到匹配的模型' : 'No matching models found';
      emptyTr.appendChild(emptyTd);
      tbody.appendChild(emptyTr);

      countDisplay.textContent = isZh()
        ? '显示 0 / ' + catalogData.totalModels + ' 个模型'
        : 'Showing 0 / ' + catalogData.totalModels + ' models';
      return;
    }

    countDisplay.textContent = isZh()
      ? '显示 ' + flatRows.length + ' / ' + catalogData.totalModels + ' 个模型'
      : 'Showing ' + flatRows.length + ' / ' + catalogData.totalModels + ' models';

    flatRows.forEach(function (item) {
      var p = item.provider;
      var m = item.model;

      var tr = document.createElement('tr');

      // 1. Model ID
      var tdModel = document.createElement('td');
      var codeModel = document.createElement('code');
      codeModel.textContent = m.id;
      tdModel.appendChild(codeModel);
      tr.appendChild(tdModel);

      // 2. Provider
      var tdProvider = document.createElement('td');
      var pTitle = document.createElement('div');
      pTitle.style.fontWeight = '600';
      pTitle.textContent = p.displayName;
      tdProvider.appendChild(pTitle);

      var pSub = document.createElement('div');
      pSub.style.fontSize = '0.85em';
      pSub.style.color = 'var(--ref-text-muted, #737373)';
      pSub.textContent = p.name;
      tdProvider.appendChild(pSub);

      if (p.requiresGateway) {
        var gwTag = document.createElement('span');
        gwTag.style.display = 'inline-block';
        gwTag.style.fontSize = '0.75rem';
        gwTag.style.padding = '0.1rem 0.35rem';
        gwTag.style.borderRadius = '4px';
        gwTag.style.background = 'rgba(234, 179, 8, 0.15)';
        gwTag.style.color = '#b45309';
        gwTag.style.marginTop = '0.2rem';
        gwTag.textContent = isZh() ? '需要网关' : 'Gateway Req.';
        tdProvider.appendChild(gwTag);
      }
      tr.appendChild(tdProvider);

      // 3. Reasoning
      var tdReasoning = document.createElement('td');
      var reasoningSpan = document.createElement('span');
      if (p.name === 'antigravity') {
        reasoningSpan.textContent = isZh() ? '支持 (可选)' : 'Supported (Opt)';
        reasoningSpan.style.color = 'var(--ref-primary, #10b981)';
        reasoningSpan.style.fontWeight = '500';
      } else {
        reasoningSpan.textContent = isZh() ? '不传递思考档位' : 'Thinking level not forwarded';
        reasoningSpan.style.color = 'var(--ref-text-muted, #737373)';
      }
      tdReasoning.appendChild(reasoningSpan);

      if (p.name === 'antigravity') {
        var noteDiv = document.createElement('div');
        noteDiv.style.fontSize = '0.75rem';
        noteDiv.style.color = 'var(--ref-text-muted, #737373)';
        noteDiv.style.marginTop = '0.2rem';
        noteDiv.textContent = isZh() ? '适配器支持/视账号' : 'Adapter opt/quota';
        tdReasoning.appendChild(noteDiv);
      }
      tr.appendChild(tdReasoning);

      // 4. Credentials
      var tdCred = document.createElement('td');
      if (p.envVars && p.envVars.length > 0) {
        p.envVars.forEach(function (env) {
          var envTag = document.createElement('div');
          var envCode = document.createElement('code');
          envCode.style.fontSize = '0.8em';
          envCode.textContent = env;
          envTag.appendChild(envCode);
          tdCred.appendChild(envTag);
        });
      } else if (p.name === 'antigravity') {
        var oAuthSpan = document.createElement('span');
        oAuthSpan.style.fontSize = '0.85em';
        oAuthSpan.textContent = isZh() ? 'OAuth凭据' : 'OAuth Credentials';
        tdCred.appendChild(oAuthSpan);
      } else {
        var noneSpan = document.createElement('span');
        noneSpan.style.fontSize = '0.85em';
        noneSpan.style.color = 'var(--ref-text-muted, #737373)';
        noneSpan.textContent = isZh() ? '无需凭据' : 'None required';
        tdCred.appendChild(noneSpan);
      }
      tr.appendChild(tdCred);

      // 5. Command
      var tdCmd = document.createElement('td');
      var cmdWrapper = document.createElement('div');
      cmdWrapper.style.display = 'flex';
      cmdWrapper.style.flexDirection = 'column';
      cmdWrapper.style.gap = '0.35rem';

      var cmdText = 'mio --provider ' + p.name + ' --model ' + m.id;
      if (p.name === 'anthropic') {
        cmdText += ' --base-url https://your-gateway.example/v1';
      }

      var cmdCode = document.createElement('code');
      cmdCode.style.fontSize = '0.8em';
      cmdCode.style.wordBreak = 'break-all';
      cmdCode.textContent = cmdText;
      cmdWrapper.appendChild(cmdCode);

      var btnRow = document.createElement('div');
      btnRow.style.display = 'flex';
      btnRow.style.alignItems = 'center';
      btnRow.style.gap = '0.5rem';

      var copyBtn = document.createElement('button');
      copyBtn.type = 'button';
      copyBtn.className = 'ref-btn ref-btn-sm';
      copyBtn.style.padding = '0.2rem 0.5rem';
      copyBtn.style.fontSize = '0.75rem';
      copyBtn.style.cursor = 'pointer';

      if (p.name === 'openrouter') {
        copyBtn.disabled = true;
        copyBtn.style.opacity = '0.6';
        copyBtn.style.cursor = 'not-allowed';
        copyBtn.textContent = isZh() ? '暂禁用复制' : 'Copy Disabled';
        btnRow.appendChild(copyBtn);

        var orNote = document.createElement('span');
        orNote.style.fontSize = '0.75rem';
        orNote.style.color = 'var(--ref-text-muted, #ef4444)';
        orNote.textContent = isZh()
          ? '嵌套模型前缀解析待修复'
          : 'Nested prefix parsing fix pending';
        btnRow.appendChild(orNote);
      } else {
        copyBtn.textContent = isZh() ? '复制命令' : 'Copy Command';
        copyBtn.addEventListener('click', function () {
          copyToClipboard(cmdText, copyBtn);
        });
        btnRow.appendChild(copyBtn);
      }

      cmdWrapper.appendChild(btnRow);
      tdCmd.appendChild(cmdWrapper);
      tr.appendChild(tdCmd);

      tbody.appendChild(tr);
    });
  }

  function updateHeadings() {
    var headings = document.querySelectorAll('th[data-zh]');
    headings.forEach(function (th) {
      th.textContent = isZh() ? th.getAttribute('data-zh') : th.getAttribute('data-en');
    });
    var labels = document.querySelectorAll('[data-i18n-zh]');
    labels.forEach(function (el) {
      var text = isZh() ? el.getAttribute('data-i18n-zh') : el.getAttribute('data-i18n-en');
      if (el.tagName === 'INPUT') el.placeholder = text; else el.textContent = text;
    });
  }

  function initEvents() {
    if (queryInput) {
      queryInput.addEventListener('input', function (e) {
        filterQuery = e.target.value;
        render();
      });
    }

    if (providerSelect) {
      providerSelect.addEventListener('change', function (e) {
        filterProvider = e.target.value;
        render();
      });
    }

    if (resetButton) {
      resetButton.addEventListener('click', function () {
        filterQuery = '';
        filterProvider = 'all';
        if (queryInput) queryInput.value = '';
        if (providerSelect) providerSelect.value = 'all';
        render();
      });
    }

    var observer = new MutationObserver(function (mutations) {
      mutations.forEach(function (mutation) {
        if (mutation.type === 'attributes' && mutation.attributeName === 'lang') {
          updateHeadings();
          if (catalogData) {
            populateProviderOptions(catalogData.providers);
          }
          render();
        }
      });
    });

    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['lang']
    });
  }

  function fetchCatalog() {
    if (!tbody || !countDisplay) return;
    countDisplay.textContent = isZh() ? '加载模型目录中...' : 'Loading model catalog...';

    fetch('model-catalog.json')
      .then(function (res) {
        if (!res.ok) {
          throw new Error('HTTP ' + res.status);
        }
        return res.json();
      })
      .then(function (data) {
        catalogData = data;
        populateProviderOptions(data.providers);
        updateHeadings();
        render();
      })
      .catch(function (err) {
        countDisplay.textContent = isZh() ? '加载模型失败' : 'Failed to load models';
        tbody.innerHTML = '';
        var tr = document.createElement('tr');
        var td = document.createElement('td');
        td.colSpan = 5;
        td.style.textAlign = 'center';
        td.style.color = '#ef4444';
        td.style.padding = '1.5rem';
        td.textContent = (isZh() ? '模型目录加载失败: ' : 'Failed to load model catalog: ') + (err && err.message ? err.message : err);
        tr.appendChild(td);
        tbody.appendChild(tr);
      });
  }

  initEvents();
  fetchCatalog();
})();
