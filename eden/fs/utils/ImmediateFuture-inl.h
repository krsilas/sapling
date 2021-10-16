/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

namespace facebook::eden {

template <typename T>
void ImmediateFuture<T>::destroy() {
  switch (kind_) {
    case Kind::Immediate:
      using TryType = folly::Try<T>;
      immediate_.~TryType();
      break;
    case Kind::SemiFuture:
      using SemiFutureType = folly::SemiFuture<T>;
      semi_.~SemiFutureType();
      break;
    case Kind::Nothing:
      break;
  }
  kind_ = Kind::Nothing;
}

template <typename T>
ImmediateFuture<T>::ImmediateFuture(folly::SemiFuture<T>&& fut) noexcept(
    std::is_nothrow_move_constructible_v<folly::SemiFuture<T>>) {
  if (fut.isReady()) {
    kind_ = Kind::Immediate;
    new (&immediate_) folly::Try<T>{std::move(fut).getTry()};
  } else {
    kind_ = Kind::SemiFuture;
    new (&semi_) folly::SemiFuture<T>{std::move(fut)};
  }
}

template <typename T>
ImmediateFuture<T>::~ImmediateFuture() {
  destroy();
}

template <typename T>
ImmediateFuture<T>::ImmediateFuture(ImmediateFuture<T>&& other) noexcept
    : kind_(other.kind_) {
  static_assert(std::is_nothrow_move_constructible_v<folly::Try<T>>);
  static_assert(std::is_nothrow_move_constructible_v<folly::SemiFuture<T>>);

  switch (kind_) {
    case Kind::Immediate:
      new (&immediate_) folly::Try<T>(std::move(other.immediate_));
      break;
    case Kind::SemiFuture:
      new (&semi_) folly::SemiFuture<T>(std::move(other.semi_));
      break;
    case Kind::Nothing:
      break;
  }
  other.kind_ = Kind::Nothing;
}

template <typename T>
ImmediateFuture<T>& ImmediateFuture<T>::operator=(
    ImmediateFuture<T>&& other) noexcept {
  static_assert(std::is_nothrow_move_constructible_v<folly::Try<T>>);
  static_assert(std::is_nothrow_move_constructible_v<folly::SemiFuture<T>>);
  if (this == &other) {
    return *this;
  }
  destroy();
  switch (other.kind_) {
    case Kind::Immediate:
      new (&immediate_) folly::Try<T>(std::move(other.immediate_));
      break;
    case Kind::SemiFuture:
      new (&semi_) folly::SemiFuture<T>(std::move(other.semi_));
      break;
    case Kind::Nothing:
      break;
  }
  kind_ = other.kind_;
  other.kind_ = Kind::Nothing;
  return *this;
}

template <typename T>
template <typename Func>
ImmediateFuture<detail::continuation_result_t<Func, T>>
ImmediateFuture<T>::thenValue(Func&& func) && {
  using RetType = detail::continuation_result_t<Func, T>;
  if (kind_ == Kind::Immediate && immediate_.hasException()) {
    return ImmediateFuture<RetType>{
        folly::Try<RetType>{std::move(immediate_).exception()}};
  }

  return std::move(*this).thenTry(
      [func = std::forward<Func>(func)](folly::Try<T>&& try_) mutable {
        // If try_ doesn't store a value, this will rethrow the exception which
        // will be caught by the thenTry method below.
        return func(std::move(try_).value());
      });
}

template <typename T>
template <typename Func>
ImmediateFuture<T> ImmediateFuture<T>::ensure(Func&& func) && {
  return std::move(*this).thenTry(
      [func = std::forward<Func>(func)](
          folly::Try<T> try_) mutable -> folly::Try<T> {
        func();
        return try_;
      });
}

template <typename T>
bool ImmediateFuture<T>::isReady() const {
  switch (kind_) {
    case Kind::Immediate:
      return true;
    case Kind::SemiFuture:
      // TODO: This could return semi_.isReady() if we also changed thenTry to
      // check semi_.isReady() and call .get() instead of .defer().
      return false;
    case Kind::Nothing:
      throw DestroyedImmediateFutureError{};
  }
}

template <typename T>
template <typename Func>
ImmediateFuture<detail::continuation_result_t<Func, folly::Try<T>>>
ImmediateFuture<T>::thenTry(Func&& func) && {
  using NewType = detail::continuation_result_t<Func, folly::Try<T>>;
  using FuncRetType = std::invoke_result_t<Func, folly::Try<T>>;

  switch (kind_) {
    case Kind::Immediate:
      try {
        // In the case where Func returns void, force the return value to
        // be folly::unit.
        if constexpr (std::is_same_v<FuncRetType, void>) {
          func(std::move(immediate_));
          return folly::unit;
        } else {
          return func(std::move(immediate_));
        }
      } catch (std::exception& ex) {
        return folly::Try<NewType>(
            folly::exception_wrapper(std::current_exception(), ex));
      }
    case Kind::SemiFuture: {
      // In the case where Func returns an ImmediateFuture, we need to
      // transform that return value into a SemiFuture so that the return
      // type is a SemiFuture<NewType> and not a
      // SemiFuture<ImmediateFuture<NewType>>.
      auto semiFut = std::move(semi_).defer(std::forward<Func>(func));
      if constexpr (detail::isImmediateFuture<FuncRetType>::value) {
        return std::move(semiFut).deferValue(
            [](auto&& immFut) { return std::move(immFut).semi(); });
      } else {
        return semiFut;
      }
    }
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename T>
T ImmediateFuture<T>::get() && {
  switch (kind_) {
    case Kind::Immediate:
      return std::move(immediate_).value();
    case Kind::SemiFuture:
      return std::move(semi_).get();
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename T>
folly::Try<T> ImmediateFuture<T>::getTry() && {
  switch (kind_) {
    case Kind::Immediate:
      return std::move(immediate_);
    case Kind::SemiFuture:
      return std::move(semi_).getTry();
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename T>
T ImmediateFuture<T>::get(folly::HighResDuration timeout) && {
  switch (kind_) {
    case Kind::Immediate:
      return std::move(immediate_).value();
    case Kind::SemiFuture:
      return std::move(semi_).get(timeout);
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename T>
folly::Try<T> ImmediateFuture<T>::getTry(folly::HighResDuration timeout) && {
  switch (kind_) {
    case Kind::Immediate:
      return std::move(immediate_);
    case Kind::SemiFuture:
      return std::move(semi_).getTry(timeout);
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename T>
folly::SemiFuture<T> ImmediateFuture<T>::semi() && {
  switch (kind_) {
    case Kind::Immediate:
      return std::move(immediate_);
    case Kind::SemiFuture:
      return std::move(semi_);
    case Kind::Nothing:
      throw DestroyedImmediateFutureError();
  }
}

template <typename Func>
auto makeImmediateFutureWith(Func&& func) {
  return ImmediateFuture<folly::Unit>().thenTry(
      [func = std::forward<Func>(func)](auto&&) mutable { return func(); });
}

template <typename T>
ImmediateFuture<std::vector<folly::Try<T>>> collectAll(
    std::vector<ImmediateFuture<T>> futures) {
  std::vector<folly::SemiFuture<T>> semis;
  std::vector<size_t> semisIndices;
  std::vector<folly::Try<T>> res;
  res.reserve(futures.size());

  size_t currentIndex = 0;
  for (auto& fut : futures) {
    if (fut.isReady()) {
      res.emplace_back(std::move(fut).getTry());
    } else {
      semis.emplace_back(std::move(fut).semi());
      semisIndices.push_back(currentIndex);
      res.emplace_back(
          folly::Try<T>{std::logic_error("Uncompleted SemiFuture")});
    }
    currentIndex++;
  }

  if (semis.empty()) {
    // All the ImmediateFuture were immediate, let's return an ImmediateFuture
    // that holds an immediate vector too.
    return std::move(res);
  }

  return folly::collectAll(std::move(semis))
      .deferValue(
          [res = std::move(res), semisIndices = std::move(semisIndices)](
              std::vector<folly::Try<T>> semisRes) mutable {
            for (size_t i = 0; i < semisRes.size(); i++) {
              res[semisIndices[i]] = std::move(semisRes[i]);
            }
            return std::move(res);
          });
}

} // namespace facebook::eden
