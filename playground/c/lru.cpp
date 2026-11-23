// Throwaway code for trying llm_lsp. Edit freely.
#include <iostream>
#include <list>
#include <optional>
#include <string>
#include <unordered_map>

template <typename K, typename V>
class LruCache {
  public:
    explicit LruCache(std::size_t capacity) : capacity_(capacity) {}

    std::optional<V> get(const K &key) {
        auto it = index_.find(key);
        if (it == index_.end())
            return std::nullopt;
        order_.splice(order_.begin(), order_, it->second);
        return it->second->second;
    }

    void put(const K &key, V value) {
        auto it = index_.find(key);
        if (it != index_.end()) {
            it->second->second = std::move(value);
            order_.splice(order_.begin(), order_, it->second);
            return;
        }
        if (order_.size() == capacity_) {
            index_.erase(order_.back().first);
            order_.pop_back();
        }
        order_.emplace_front(key, std::move(value));
        index_[key] = order_.begin();
    }

    std::size_t size() const { return order_.size(); }

  private:
    std::size_t capacity_;
    std::list<std::pair<K, V>> order_;
    std::unordered_map<K, typename std::list<std::pair<K, V>>::iterator> index_;
};

int main() {
    LruCache<std::string, int> cache(2);
    cache.put("a", 1);
    cache.put("b", 2);
    cache.get("a");
    cache.put("c", 3); // evicts "b"
    std::cout << "b present: " << cache.get("b").has_value() << "\n";
    std::cout << "size: " << cache.size() << "\n";
}
