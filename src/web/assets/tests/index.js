// `node --test src/web/assets/tests/` names this directory, and Node (22 and later) runs a
// directory argument as one module: this file. So it pulls in every test file.
// `node --test "src/web/assets/tests/*.test.js"` runs them one process each instead; both
// run the same tests. csp.test.js checks that this list has every *.test.js file.
import './api.test.js';
import './browse-api.test.js';
import './browse-mock.test.js';
import './browse-route.test.js';
import './browse.test.js';
import './chrome.test.js';
import './confirm.test.js';
import './csp.test.js';
import './edits-api.test.js';
import './edits-mock.test.js';
import './edits.test.js';
import './format.test.js';
import './forms.test.js';
import './history.test.js';
import './keys.test.js';
import './layouts.test.js';
import './layouts-review.test.js';
import './mock.test.js';
import './p4-api.test.js';
import './p4-mock.test.js';
import './p4-views.test.js';
import './signin.test.js';
import './tail.test.js';
import './theme.test.js';
import './views.test.js';
