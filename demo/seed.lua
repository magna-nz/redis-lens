-- Seeds a keyspace shaped like a real application cache, with problems planted for
-- redis-lens to find:
--   user:{id}:cart       about 31% of carts never expire (a write path skips EXPIRE)
--   cache:search:{hash}  about 12% never expire, and those are the biggest
--   tmp:import:{ulid}    an import job that never cleans up: nothing expires
--   queue:emails         one very large list
--   feature:checkout-v2  a hot key (needs an LFU eviction policy to show up)
--
-- Usage: redis-cli --eval seed.lua , <scale>
-- Scale 1 writes about 100K keys and uses about 200 MB.

local scale = tonumber(ARGV[1] or "1")
local function n(x) return math.max(1, math.floor(x * scale)) end
local function between(a, b) return math.random(a, b) end

local HEX = "0123456789abcdef"
local function hex(len)
  local t = {}
  for i = 1, len do
    local r = math.random(16)
    t[i] = HEX:sub(r, r)
  end
  return table.concat(t)
end

local function uuid()
  return hex(8) .. "-" .. hex(4) .. "-4" .. hex(3) .. "-a" .. hex(3) .. "-" .. hex(12)
end

local CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
local function ulid()
  local t = { "01J" }
  for i = 1, 23 do
    local r = math.random(32)
    t[#t + 1] = CROCKFORD:sub(r, r)
  end
  return table.concat(t)
end

local function blob(size) return string.rep("x", size) end

local written = 0
local function set(key, size, ttl)
  if ttl then
    redis.call("SET", key, blob(size), "EX", ttl)
  else
    redis.call("SET", key, blob(size))
  end
  written = written + 1
end

local HOUR = 3600
local DAY = 86400

-- Sessions: every one expires within a day.
for _ = 1, n(20000) do
  set("session:" .. uuid(), between(2000, 4500), between(600, DAY))
end

-- Search results: 12% never expire, and those are bigger.
for _ = 1, n(10000) do
  if math.random() < 0.12 then
    set("cache:search:" .. hex(40), between(4000, 8000), nil)
  else
    set("cache:search:" .. hex(40), between(1000, 2500), between(HOUR, DAY))
  end
end

-- Product pages: all expire within a day.
for i = 1, n(5000) do
  set("cache:product:" .. i, between(4000, 8000), between(HOUR, DAY))
end

-- Users: persistent profiles, carts that should expire in 5-7 days, feeds that do.
for i = 1, n(9000) do
  local uid = 10000 + i * 7
  redis.call("HSET", "user:" .. uid .. ":profile",
    "name", "user" .. uid, "email", "u" .. uid .. "@example.com", "bio", blob(between(200, 1200)))
  written = written + 1

  if i % 9 ~= 0 then
    local cart = "user:" .. uid .. ":cart"
    for _ = 1, between(1, 6) do
      redis.call("HSET", cart, "sku:" .. between(1, 5000), blob(between(200, 600)))
    end
    if math.random() >= 0.31 then
      redis.call("EXPIRE", cart, between(5 * DAY, 7 * DAY))
    end
    written = written + 1
  end

  if i % 4 == 0 then
    local feed = "user:" .. uid .. ":feed"
    for _ = 1, between(5, 30) do
      redis.call("ZADD", feed, between(1, 1000000000), "post:" .. between(1, 1000000))
    end
    redis.call("EXPIRE", feed, between(DAY, 7 * DAY))
    written = written + 1
  end
end

-- Queues: no TTL, as expected for queues. queue:emails is very large.
local queues = { emails = 20000, webhooks = 5000, thumbnails = 1500, ["search-index"] = 300, audit = 50 }
for name, items in pairs(queues) do
  local key = "queue:" .. name
  local batch = {}
  for _ = 1, n(items) do
    batch[#batch + 1] = blob(between(600, 1400))
    if #batch == 500 then
      redis.call("RPUSH", key, unpack(batch))
      batch = {}
    end
  end
  if #batch > 0 then redis.call("RPUSH", key, unpack(batch)) end
  written = written + 1
end

-- An import job that never cleans up after itself.
for _ = 1, n(1500) do
  set("tmp:import:" .. ulid(), between(8000, 20000), nil)
end

-- Rate-limit windows: tiny, short-lived.
for _ = 1, n(30000) do
  local ip = "10." .. between(0, 255) .. "." .. between(0, 255) .. "." .. between(1, 254)
  set("rl:" .. ip .. ":" .. between(29187400, 29187460), between(8, 16), between(300, 900))
end

-- Feature flags: persistent configuration, as intended.
for _, f in ipairs({ "checkout-v2", "dark-mode", "pricing-rules", "search-ranker", "beta-banner" }) do
  set("feature:" .. f, between(40, 4000), nil)
end
for i = 1, 200 do set("feature:flag-" .. i, between(40, 200), nil) end

-- Locks: all with a TTL.
for _, l in ipairs({ "billing-run", "nightly-export", "reindex-products", "sitemap" }) do
  set("lock:" .. l, 150, between(60, 1800))
end

-- Traffic for the hot-keys view. LFU counters grow logarithmically, and EXISTS does
-- not touch them, so read with GET (LLEN for the list).
local hot = {
  { "feature:checkout-v2", 8000 },
  { "cache:product:1", 4000 },
  { "queue:emails", 2000 },
  { "cache:product:2", 1000 },
  { "feature:dark-mode", 500 },
  { "lock:billing-run", 250 },
}
for _, entry in ipairs(hot) do
  local key, reads = entry[1], entry[2]
  local read = key:sub(1, 6) == "queue:" and "LLEN" or "GET"
  for _ = 1, reads do redis.call(read, key) end
end

return written
